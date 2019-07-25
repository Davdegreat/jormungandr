/*

```text
          +------------+                     +------------+                    +------------+
          | Leadership |                     | Leadership |                    | Leadership |
          +-----+------+                     +-----+------+                    +-------+----+
                ^                                  ^                                   ^
                |                                  |                                   |
      +---------v-----^--------------+             +<------------+                +--->+--------+
      |               |              |             |             |                |             |
      |               |              |             |             |                |             |
   +--+--+         +--+--+        +--+--+       +--+--+       +--+--+          +--+--+       +--+--+
   | Ref +<--------+ Ref +<-------+ Ref +<--+---+ Ref +<------+ Ref +<---------+ Ref +<------+ Ref |
   +--+--+         +--+--+        +--+--+   ^   +--+--+       +--+--+          +---+-+       +---+-+
      |               |              |      |      |             |                 |             |
      v               v              v      |      v             v                 v             v
+-----+--+      +-----+--+       +---+----+ |   +--+-----+   +---+----+      +-----+--+       +--+-----+
| Ledger |      | Ledger |       | Ledger | |   | Ledger |   | Ledger |      | Ledger |       | Ledger |
+--------+      +--------+       +--------+ |   +--------+   +--------+      +--------+       +--------+
                                            |
                                            |
                                            |parent
                                            |hash
                                            |
                                            |         +----------+
                                            +---------+New header|
                                                      +----------+
```

When proposing a new header to the blockchain we are creating a new
potential fork on the blockchain. In the ideal case it will simply be
a new block on top of the _main_ current branch. We are adding blocks
after the other. In some cases it will also be a new branch, a fork.
We need to maintain some of them in order to be able to make an
informed choice when selecting the branch of consensus.

We are constructing a blockchain as we would on with git blocks:

* each block is represented by a [`Ref`];
* the [`Ref`] contains a reference to the associated `Ledger` state
  and associated `Leadership`.

A [`Branch`] contains a [`Ref`]. It allows us to follow and monitor
forks between different tasks of the blockchain module.

[`Ref`]: ./struct.Ref.html
[`Branch`]: ./struct.Branch.html
*/

mod branch;
mod multiverse;
mod reference;
mod reference_cache;
mod storage;

pub use self::{
    branch::{Branch, Branches},
    multiverse::Multiverse,
    reference::Ref,
    reference_cache::RefCache,
    storage::Storage,
};
use crate::{
    blockcfg::{Block, Block0Error, Header, HeaderHash, Leadership, Ledger},
    start_up::NodeStorage,
};
use chain_impl_mockchain::{leadership::Verification, ledger};
use chain_storage::error::Error as StorageError;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::prelude::*;

error_chain! {
    foreign_links {
        Storage(StorageError);
        Ledger(ledger::Error);
        Block0(Block0Error);
    }

    errors {
        Block0InitialLedgerError {
            description("Error while creating the initial ledger out of the block0")
        }

        Block0AlreadyInStorage {
            description("Block0 already exists in the storage")
        }

        Block0NotAlreadyInStorage {
            description("Block0 already is not yet in the storage")
        }

        NoTag (tag: String) {
            description("Missing tag from the storage"),
            display("Tag '{}' not found in the storage", tag),
        }

        BLockHeaderVerificationFail (reason: String) {
            description("Block header verification failed"),
            display("The block header verification failed: {}", reason),
        }

        CannotApplyBlock {
            description("Block cannot be applied on top of the previous block's ledger state"),
        }
    }
}

const MAIN_BRANCH_TAG: &str = "HEAD";

#[derive(Clone)]
pub struct Blockchain {
    branches: Branches,

    ref_cache: RefCache,

    multiverse: Multiverse<Ledger>,

    leaderships: Multiverse<Arc<Leadership>>,

    storage: Storage,
}

pub enum PreCheckedHeader {
    /// result when the given header is already present in the
    /// local storage. The embedded `cached_reference` gives us
    /// the local cached reference is the header is already loaded
    /// in memory
    AlreadyPresent {
        /// return the Header so we can avoid doing clone
        /// of the data all the time
        header: Header,
        /// the cached reference if it was already cached.
        /// * `None` means the associated block is already in storage
        ///   but not already in the cache;
        /// * `Some(ref)` returns the local cached Ref of the block
        cached_reference: Option<Ref>,
    },

    /// the parent is missing from the local storage
    MissingParent {
        /// return back the Header so we can avoid doing a clone
        /// of the data all the time
        header: Header,
    },

    /// The parent's is already present in the local storage and
    /// is loaded in the local cache
    HeaderWithCache {
        /// return back the Header so we can avoid doing a clone
        /// of the data all the time
        header: Header,

        /// return the locally stored parent's Ref. Already cached in memory
        /// for future processing
        parent_ref: Ref,
    },
}

pub struct PostCheckedHeader {
    header: Header,
    leadership_schedule: Arc<Leadership>,
    parent_ledger_state: Ledger,
}

impl Blockchain {
    pub fn new(storage: NodeStorage, ref_cache_ttl: Duration) -> Self {
        Blockchain {
            branches: Branches::new(),
            ref_cache: RefCache::new(ref_cache_ttl),
            multiverse: Multiverse::new(),
            leaderships: Multiverse::new(),
            storage: Storage::new(storage),
        }
    }

    /// create and store a reference of this leader to the new
    fn create_and_store_reference(
        &mut self,
        header_hash: HeaderHash,
        header: Header,
        ledger: Ledger,
        leadership: Arc<Leadership>,
    ) -> impl Future<Item = Ref, Error = Infallible> {
        let chain_length = header.chain_length();

        let leaderships = self.leaderships.clone();
        let multiverse = self.multiverse.clone();
        let ref_cache = self.ref_cache.clone();

        multiverse
            .insert(chain_length, header_hash, ledger)
            .and_then(move |ledger_gcroot| {
                leaderships
                    .insert(chain_length, header_hash, leadership)
                    .map(|leadership_gcroot| (ledger_gcroot, leadership_gcroot))
            })
            .and_then(move |(ledger_gcroot, leadership_gcroot)| {
                let reference = Ref::new(ledger_gcroot, leadership_gcroot, header);
                ref_cache
                    .insert(header_hash, reference.clone())
                    .map(|()| reference)
            })
    }

    /// get `Ref` of the given header hash
    ///
    /// once the `Ref` is in hand, it means we have the Leadership schedule associated
    /// to this block and the `Ledger` state after this block.
    ///
    /// If the future returns `None` it means we don't know about this block locally
    /// and it might be necessary to contacts the network to retrieve a missing
    /// branch
    ///
    /// TODO: the case where the block is in storage but not yet in the cache
    ///       is not implemented
    pub fn get_ref(
        &mut self,
        header_hash: HeaderHash,
    ) -> impl Future<Item = Option<Ref>, Error = Error> {
        let get_ref_cache_future = self.ref_cache.get(header_hash.clone());
        let block_exists_future = self.storage.block_exists(header_hash);

        get_ref_cache_future
            .map_err(|_: Infallible| unreachable!())
            .and_then(|maybe_ref| {
                if maybe_ref.is_none() {
                    future::Either::A(
                        block_exists_future
                            .map_err(|e| {
                                Error::with_chain(e, "cannot check if the block is in the storage")
                            })
                            .and_then(|block_exists| {
                                if block_exists {
                                    unimplemented!(
                                        "method to load a Ref from the storage is not yet there"
                                    )
                                } else {
                                    future::ok(None)
                                }
                            }),
                    )
                } else {
                    future::Either::B(future::ok(maybe_ref))
                }
            })
    }

    /// load the header's parent `Ref`.
    fn load_header_parent(
        &mut self,
        header: Header,
    ) -> impl Future<Item = PreCheckedHeader, Error = Error> {
        let block_id = header.hash();
        let parent_block_id = header.block_parent_hash().clone();

        let get_self_ref = self.get_ref(block_id.clone());
        let get_parent_ref = self.get_ref(parent_block_id);

        get_self_ref.and_then(|maybe_self_ref| {
            if let Some(self_ref) = maybe_self_ref {
                future::Either::A(future::ok(PreCheckedHeader::AlreadyPresent {
                    header,
                    cached_reference: Some(self_ref),
                }))
            } else {
                future::Either::B(get_parent_ref.and_then(|maybe_parent_ref| {
                    if let Some(parent_ref) = maybe_parent_ref {
                        future::ok(PreCheckedHeader::HeaderWithCache { header, parent_ref })
                    } else {
                        future::ok(PreCheckedHeader::MissingParent { header })
                    }
                }))
            }
        })
    }

    /// load the header's parent and perform some simple verification:
    ///
    /// * check the block_date is increasing
    /// * check the chain_length is monotonically increasing
    ///
    /// At the end of this future we know either of:
    ///
    /// * the block is already present (nothing to do);
    /// * the block's parent is already present (we can then continue validation)
    /// * the block's parent is missing: we need to download it and call again
    ///   this function.
    ///
    pub fn pre_check_header(
        &mut self,
        header: Header,
    ) -> impl Future<Item = PreCheckedHeader, Error = Error> {
        // TODO: before loading the parent's header we can check
        //       the crypto of the header (i.e. check that they
        //       actually sign the header signing data against
        //       the public key).

        self.load_header_parent(header)
            .and_then(|pre_check| match &pre_check {
                PreCheckedHeader::HeaderWithCache {
                    ref header,
                    ref parent_ref,
                } => {
                    use chain_core::property::ChainLength as _;

                    if header.block_date() <= parent_ref.block_date() {
                        return future::err(
                            "block is not valid, date is set before parent's".into(),
                        );
                    }
                    if header.chain_length().next() != parent_ref.chain_length() {
                        return future::err(
                            "block is not valid, chain length is not monotonically increasing"
                                .into(),
                        );
                    }
                    // TODO: check the crypto of the header

                    future::ok(pre_check)
                }
                _ => future::ok(pre_check),
            })
    }

    fn get_ledger_state(
        &mut self,
        header_hash: HeaderHash,
    ) -> impl Future<Item = Ledger, Error = Error> {
        self.multiverse
            .get(header_hash)
            .map_err(|_: Infallible| unreachable!())
            .and_then(|opt_ledger| {
                if let Some(ledger) = opt_ledger {
                    future::ok(ledger)
                } else {
                    unimplemented!("missing implementation to load ledger state from storage")
                }
            })
    }

    fn get_leadership_state(
        &mut self,
        header_hash: HeaderHash,
    ) -> impl Future<Item = Arc<Leadership>, Error = Error> {
        self.leaderships
            .get(header_hash)
            .map_err(|_: Infallible| unreachable!())
            .and_then(|opt_leadership| {
                if let Some(leadership) = opt_leadership {
                    future::ok(leadership)
                } else {
                    unimplemented!("missing implementation to load leadership state from storage")
                }
            })
    }

    /// check the header cryptographic properties and leadership's schedule
    ///
    /// on success returns the PostCheckedHeader:
    ///
    /// * the header,
    /// * the ledger state associated to the parent block
    /// * the leadership schedule associated to the header
    pub fn post_check_header(
        &mut self,
        header: Header,
        parent: Ref,
    ) -> impl Future<Item = PostCheckedHeader, Error = Error> {
        let parent_ledger_state = self.get_ledger_state(parent.hash().clone());
        let parent_leadership = self.get_leadership_state(parent.hash().clone());

        parent_ledger_state
            .and_then(|parent_ledger_state| {
                parent_leadership.map(|parent_leadership| (parent_ledger_state, parent_leadership))
            })
            .and_then(move |(parent_ledger_state, parent_leadership_schedule)| {
                let current_date = header.block_date();
                let parent_date = parent.block_date();

                let leadership_schedule = if parent_date.epoch < current_date.epoch {
                    Arc::new(Leadership::new(current_date.epoch, &parent_ledger_state))
                } else {
                    parent_leadership_schedule
                };

                match leadership_schedule.verify(&header) {
                    Verification::Success => future::ok(PostCheckedHeader {
                        header,
                        leadership_schedule,
                        parent_ledger_state,
                    }),
                    Verification::Failure(error) => future::err(
                        ErrorKind::BLockHeaderVerificationFail(error.to_string()).into(),
                    ),
                }
            })
    }

    /// TODO:
    ///
    /// apply the block on the blockchain from a post checked header
    ///
    pub fn apply_block(
        &mut self,
        post_checked_header: PostCheckedHeader,
        block: Block,
    ) -> impl Future<Item = Ref, Error = Error> {
        let header = post_checked_header.header;
        let block_id = header.hash();
        let leadership = post_checked_header.leadership_schedule;
        let ledger = post_checked_header.parent_ledger_state;

        debug_assert!(block.header.hash() == block_id);

        let metadata = header.to_content_eval_context();

        let mut self1 = self.clone();

        future::result(
            ledger
                .apply_block(
                    leadership.ledger_parameters(),
                    block.contents.iter(),
                    &metadata,
                )
                .chain_err(|| ErrorKind::CannotApplyBlock),
        )
        .and_then(move |new_ledger| {
            self1
                .create_and_store_reference(block_id, header, new_ledger, leadership)
                .map_err(|_: Infallible| unreachable!())
        })
    }

    /// Apply the given block0 in the blockchain (updating the RefCache and the other objects)
    ///
    /// This function returns the created block0 branch. Having it will
    /// avoid searching for it in the blockchain's `branches` and perform
    /// operations to update the branch as we move along already.
    ///
    /// # Errors
    ///
    /// The resulted future may fail if
    ///
    /// * the block0 does build an invalid `Ledger`: `ErrorKind::Block0InitialLedgerError`;
    ///
    fn apply_block0(&mut self, block0: Block) -> impl Future<Item = Branch, Error = Error> {
        let block0_header = block0.header.clone();
        let block0_id = block0_header.hash();
        let block0_id_1 = block0_header.hash();
        let block0_date = block0_header.block_date().clone();

        let mut self1 = self.clone();
        let mut branches = self.branches.clone();

        // we lift the creation of the ledger in the future type
        // this allow chaining of the operation and lifting the error handling
        // in the same place
        Ledger::new(block0_id_1, block0.contents.iter())
            .map(future::ok)
            .map_err(|err| Error::with_chain(err, ErrorKind::Block0InitialLedgerError))
            .unwrap_or_else(future::err)
            .map(move |block0_ledger| {
                let block0_leadership = Leadership::new(block0_date.epoch, &block0_ledger);
                (block0_ledger, block0_leadership)
            })
            .and_then(move |(block0_ledger, block0_leadership)| {
                self1
                    .create_and_store_reference(
                        block0_id,
                        block0_header,
                        block0_ledger,
                        Arc::new(block0_leadership),
                    )
                    .map_err(|_: Infallible| unreachable!())
            })
            .map(Branch::new)
            .and_then(move |branch| {
                branches
                    .add(branch.clone())
                    .map(|()| branch)
                    .map_err(|_: Infallible| unreachable!())
            })
    }

    /// function to do the initial application of the block0 in the `Blockchain` and its
    /// storage. We assume `Block0` is not already in the `NodeStorage`.
    ///
    /// This function returns the create block0 branch. Having it will
    /// avoid searching for it in the blockchain's `branches` and perform
    /// operations to update the branch as we move along already.
    ///
    /// # Errors
    ///
    /// The resulted future may fail if
    ///
    /// * the block0 already exists in the storage: `ErrorKind::Block0AlreadyInStorage`;
    /// * the block0 does build a valid `Ledger`: `ErrorKind::Block0InitialLedgerError`;
    /// * other errors while interacting with the storage (IO errors)
    ///
    pub fn load_from_block0(&mut self, block0: Block) -> impl Future<Item = Branch, Error = Error> {
        let block0_clone = block0.clone();
        let block0_header = block0.header.clone();
        let block0_id = block0_header.hash();

        let mut self1 = self.clone();
        let mut storage_store = self.storage.clone();
        let mut storage_store_2 = self.storage.clone();

        self.storage
            .block_exists(block0_id.clone())
            .map_err(|e| Error::with_chain(e, "Cannot check if block0 is in storage"))
            .and_then(|existence| {
                if !existence {
                    future::err(ErrorKind::Block0AlreadyInStorage.into())
                } else {
                    future::ok(())
                }
            })
            .and_then(move |()| self1.apply_block0(block0_clone))
            .and_then(move |block0_branch| {
                storage_store
                    .put_block(block0)
                    .map(|()| block0_branch)
                    .map_err(|e| Error::with_chain(e, "Cannot put block0 in storage"))
            })
            .and_then(move |block0_branch| {
                storage_store_2
                    .put_tag(MAIN_BRANCH_TAG.to_owned(), block0_id)
                    .map(|()| block0_branch)
                    .map_err(|e| Error::with_chain(e, "Cannot put block0's hash in the HEAD tag"))
            })
    }

    /// returns a future that will propagate the initial states and leadership
    /// from the block0 to the `Head` of the storage (the last known block which
    /// made consensus).
    ///
    /// The Future will returns a branch pointing to the `Head`.
    ///
    /// # Errors
    ///
    /// The resulted future may fail if
    ///
    /// * the block0 is not already in the storage: `ErrorKind::Block0NotAlreadyInStorage`;
    /// * the block0 does build a valid `Ledger`: `ErrorKind::Block0InitialLedgerError`;
    /// * other errors while interacting with the storage (IO errors)
    ///
    pub fn load_from_storage(
        &mut self,
        block0: Block,
    ) -> impl Future<Item = Branch, Error = Error> {
        let block0_header = block0.header.clone();
        let block0_id = block0_header.hash();
        let block0_id_2 = block0_id.clone();
        let self1 = self.clone();
        let mut self2 = self.clone();
        let self3 = self.clone();
        let self4 = self.clone();

        self.storage
            .block_exists(block0_id.clone())
            .map_err(|e| Error::with_chain(e, "Cannot check if block0 is in storage"))
            .and_then(|existence| {
                if existence {
                    future::err(ErrorKind::Block0NotAlreadyInStorage.into())
                } else {
                    future::ok(())
                }
            })
            .and_then(move |()| {
                self1
                    .storage
                    .get_tag(MAIN_BRANCH_TAG.to_owned())
                    .map_err(|e| Error::with_chain(e, "Cannot get hash of the HEAD tag"))
                    .and_then(|opt| {
                        if let Some(id) = opt {
                            future::ok(id)
                        } else {
                            future::err(ErrorKind::NoTag(MAIN_BRANCH_TAG.to_owned()).into())
                        }
                    })
            })
            .and_then(move |head_hash| {
                self2
                    .apply_block0(block0)
                    .map(move |branch| (branch, head_hash))
            })
            .and_then(move |(branch, head_hash)| {
                // TODO:
                // 1. [x] stream for every blocks between block0_id and head_hash
                // 2. [x] for every block, apply blockchain on top of branch
                // 3. [ ] run GC so we don't keep things we don't need

                self3
                    .storage
                    .stream_from_to(block0_id_2, head_hash)
                    .map_err(|e| Error::with_chain(e, "Cannot iterate blocks from block0 to HEAD"))
                    .and_then(|block_stream| {
                        if let Some(block_stream) = block_stream {
                            future::ok(block_stream)
                        } else {
                            future::err("Cannot iterate between block0 and HEAD".into())
                        }
                    })
                    .and_then(move |block_stream| {
                        block_stream
                            .map_err(|e| {
                                Error::with_chain(e, "Error while iterating between bloc0 and HEAD")
                            })
                            .fold((branch, self4), move |(branch, mut self4), block: Block| {
                                let header = block.header.clone();

                                let mut self5 = self4.clone();
                                let mut self6 = self4.clone();
                                let returned = self4.clone();

                                self4
                                    .pre_check_header(header)
                                    .and_then(move |pre_checked_header: PreCheckedHeader| {
                                        match pre_checked_header {
                                            PreCheckedHeader::HeaderWithCache {
                                                header,
                                                parent_ref,
                                            } => self5.post_check_header(header, parent_ref),
                                            _ => unimplemented!(),
                                        }
                                    })
                                    .and_then(move |post_checked_header: PostCheckedHeader| {
                                        self6.apply_block(post_checked_header, block)
                                    })
                                    .and_then(move |new_ref| {
                                        branch
                                            .clone()
                                            .update_ref(new_ref)
                                            .map(move |_old_ref| (branch, returned))
                                            .map_err(|_: Infallible| unreachable!())
                                    })
                            })
                            .map(|(branch, _)| branch)
                    })
            })
    }
}
