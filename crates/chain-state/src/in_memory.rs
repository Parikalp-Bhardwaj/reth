//! Types for tracking the canonical chain state in memory.

use crate::{
    CanonStateNotification, CanonStateNotificationSender, CanonStateNotifications, ChainInfoTracker,
};
use parking_lot::RwLock;
use reth_chainspec::ChainInfo;
use reth_execution_types::{Chain, ExecutionOutcome};
use reth_primitives::{
    Address, BlockNumHash, Header, Receipt, Receipts, SealedBlock, SealedBlockWithSenders,
    SealedHeader, B256,
};
use reth_trie::{updates::TrieUpdates, HashedPostState};
use std::{collections::HashMap, ops::Deref, sync::Arc, time::Instant};
use tokio::sync::broadcast;

/// Size of the broadcast channel used to notify canonical state events.
const CANON_STATE_NOTIFICATION_CHANNEL_SIZE: usize = 256;

/// Container type for in memory state data of the canonical chain.
///
/// This tracks blocks and their state that haven't been persisted to disk yet but are part of the
/// canonical chain that can be traced back to a canonical block on disk.
#[derive(Debug, Default)]
pub(crate) struct InMemoryState {
    /// All canonical blocks that are not on disk yet.
    blocks: RwLock<HashMap<B256, Arc<BlockState>>>,
    /// Mapping of block numbers to block hashes.
    numbers: RwLock<HashMap<u64, B256>>,
    /// The pending block that has not yet been made canonical.
    pending: RwLock<Option<BlockState>>,
}

impl InMemoryState {
    pub(crate) const fn new(
        blocks: HashMap<B256, Arc<BlockState>>,
        numbers: HashMap<u64, B256>,
        pending: Option<BlockState>,
    ) -> Self {
        Self {
            blocks: RwLock::new(blocks),
            numbers: RwLock::new(numbers),
            pending: RwLock::new(pending),
        }
    }

    /// Returns the state for a given block hash.
    pub(crate) fn state_by_hash(&self, hash: B256) -> Option<Arc<BlockState>> {
        self.blocks.read().get(&hash).cloned()
    }

    /// Returns the state for a given block number.
    pub(crate) fn state_by_number(&self, number: u64) -> Option<Arc<BlockState>> {
        self.numbers.read().get(&number).and_then(|hash| self.blocks.read().get(hash).cloned())
    }

    /// Returns the current chain head state.
    pub(crate) fn head_state(&self) -> Option<Arc<BlockState>> {
        self.numbers
            .read()
            .iter()
            .max_by_key(|(&number, _)| number)
            .and_then(|(_, hash)| self.blocks.read().get(hash).cloned())
    }

    /// Returns the pending state corresponding to the current head plus one,
    /// from the payload received in newPayload that does not have a FCU yet.
    pub(crate) fn pending_state(&self) -> Option<Arc<BlockState>> {
        self.pending.read().as_ref().map(|state| Arc::new(BlockState::new(state.block.clone())))
    }

    #[cfg(test)]
    fn block_count(&self) -> usize {
        self.blocks.read().len()
    }
}

/// Inner type to provide in memory state. It includes a chain tracker to be
/// advanced internally by the tree.
#[derive(Debug)]
pub(crate) struct CanonicalInMemoryStateInner {
    pub(crate) chain_info_tracker: ChainInfoTracker,
    pub(crate) in_memory_state: InMemoryState,
    pub(crate) canon_state_notification_sender: CanonStateNotificationSender,
}

/// This type is responsible for providing the blocks, receipts, and state for
/// all canonical blocks not on disk yet and keeps track of the block range that
/// is in memory.
#[derive(Debug, Clone)]
pub struct CanonicalInMemoryState {
    pub(crate) inner: Arc<CanonicalInMemoryStateInner>,
}

impl CanonicalInMemoryState {
    /// Create a new in memory state with the given blocks, numbers, and pending state.
    pub fn new(
        blocks: HashMap<B256, Arc<BlockState>>,
        numbers: HashMap<u64, B256>,
        pending: Option<BlockState>,
    ) -> Self {
        let in_memory_state = InMemoryState::new(blocks, numbers, pending);
        let head_state = in_memory_state.head_state();
        let header = match head_state {
            Some(state) => state.block().block().header.clone(),
            None => SealedHeader::default(),
        };
        let chain_info_tracker = ChainInfoTracker::new(header);
        let (canon_state_notification_sender, _canon_state_notification_receiver) =
            broadcast::channel(CANON_STATE_NOTIFICATION_CHANNEL_SIZE);

        let inner = CanonicalInMemoryStateInner {
            chain_info_tracker,
            in_memory_state,
            canon_state_notification_sender,
        };

        Self { inner: Arc::new(inner) }
    }

    /// Create a new in memory state with the given local head.
    pub fn with_head(head: SealedHeader) -> Self {
        let chain_info_tracker = ChainInfoTracker::new(head);
        let in_memory_state = InMemoryState::default();
        let (canon_state_notification_sender, _canon_state_notification_receiver) =
            broadcast::channel(CANON_STATE_NOTIFICATION_CHANNEL_SIZE);
        let inner = CanonicalInMemoryStateInner {
            chain_info_tracker,
            in_memory_state,
            canon_state_notification_sender,
        };

        Self { inner: Arc::new(inner) }
    }

    /// Append new blocks to the in memory state.
    fn update_blocks<I>(&self, new_blocks: I, reorged: I)
    where
        I: IntoIterator<Item = ExecutedBlock>,
    {
        // acquire all locks
        let mut blocks = self.inner.in_memory_state.blocks.write();
        let mut numbers = self.inner.in_memory_state.numbers.write();
        let mut pending = self.inner.in_memory_state.pending.write();

        // we first remove the blocks from the reorged chain
        for block in reorged {
            let hash = block.block().hash();
            let number = block.block().number;
            blocks.remove(&hash);
            numbers.remove(&number);
        }

        // insert the new blocks
        for block in new_blocks {
            let parent = blocks.get(&block.block().parent_hash).cloned();
            let block_state = BlockState::with_parent(block.clone(), parent.map(|p| (*p).clone()));
            let hash = block_state.hash();
            let number = block_state.number();

            // append new blocks
            blocks.insert(hash, Arc::new(block_state));
            numbers.insert(number, hash);
        }

        // remove the pending state
        pending.take();
    }

    /// Update the in memory state with the given chain update.
    pub fn update_chain(&self, new_chain: NewCanonicalChain) {
        match new_chain {
            NewCanonicalChain::Commit { new } => {
                self.update_blocks(new, vec![]);
            }
            NewCanonicalChain::Reorg { new, old } => {
                self.update_blocks(new, old);
            }
        }
    }

    /// Returns in memory state corresponding the given hash.
    pub fn state_by_hash(&self, hash: B256) -> Option<Arc<BlockState>> {
        self.inner.in_memory_state.state_by_hash(hash)
    }

    /// Returns in memory state corresponding the block number.
    pub fn state_by_number(&self, number: u64) -> Option<Arc<BlockState>> {
        self.inner.in_memory_state.state_by_number(number)
    }

    /// Returns the in memory head state.
    pub fn head_state(&self) -> Option<Arc<BlockState>> {
        self.inner.in_memory_state.head_state()
    }

    /// Returns the in memory pending state.
    pub fn pending_state(&self) -> Option<Arc<BlockState>> {
        self.inner.in_memory_state.pending_state()
    }

    /// Returns the in memory pending `BlockNumHash`.
    pub fn pending_block_num_hash(&self) -> Option<BlockNumHash> {
        self.inner
            .in_memory_state
            .pending_state()
            .map(|state| BlockNumHash { number: state.number(), hash: state.hash() })
    }

    /// Returns the current `ChainInfo`.
    pub fn chain_info(&self) -> ChainInfo {
        self.inner.chain_info_tracker.chain_info()
    }

    /// Returns the latest canonical block number.
    pub fn get_canonical_block_number(&self) -> u64 {
        self.inner.chain_info_tracker.get_canonical_block_number()
    }

    /// Returns the `BlockNumHash` of the safe head.
    pub fn get_safe_num_hash(&self) -> Option<BlockNumHash> {
        self.inner.chain_info_tracker.get_safe_num_hash()
    }

    /// Returns the `BlockNumHash` of the finalized head.
    pub fn get_finalized_num_hash(&self) -> Option<BlockNumHash> {
        self.inner.chain_info_tracker.get_finalized_num_hash()
    }

    /// Hook for new fork choice update.
    pub fn on_forkchoice_update_received(&self) {
        self.inner.chain_info_tracker.on_forkchoice_update_received();
    }

    /// Returns the timestamp of the last received update.
    pub fn last_received_update_timestamp(&self) -> Option<Instant> {
        self.inner.chain_info_tracker.last_forkchoice_update_received_at()
    }

    /// Hook for transition configuration exchanged.
    pub fn on_transition_configuration_exchanged(&self) {
        self.inner.chain_info_tracker.on_transition_configuration_exchanged();
    }

    /// Returns the timepstamp of the last transition configuration exchanged,
    pub fn last_exchanged_transition_configuration_timestamp(&self) -> Option<Instant> {
        self.inner.chain_info_tracker.last_transition_configuration_exchanged_at()
    }

    /// Canonical head setter.
    pub fn set_canonical_head(&self, header: SealedHeader) {
        self.inner.chain_info_tracker.set_canonical_head(header);
    }

    /// Safe head setter.
    pub fn set_safe(&self, header: SealedHeader) {
        self.inner.chain_info_tracker.set_safe(header);
    }

    /// Finalized head setter.
    pub fn set_finalized(&self, header: SealedHeader) {
        self.inner.chain_info_tracker.set_finalized(header);
    }

    /// Canonical head getter.
    pub fn get_canonical_head(&self) -> SealedHeader {
        self.inner.chain_info_tracker.get_canonical_head()
    }

    /// Finalized header getter.
    pub fn get_finalized_header(&self) -> Option<SealedHeader> {
        self.inner.chain_info_tracker.get_finalized_header()
    }

    /// Safe header getter.
    pub fn get_safe_header(&self) -> Option<SealedHeader> {
        self.inner.chain_info_tracker.get_safe_header()
    }

    /// Returns the `SealedHeader` corresponding to the pending state.
    pub fn pending_sealed_header(&self) -> Option<SealedHeader> {
        self.pending_state().map(|h| h.block().block().header.clone())
    }

    /// Returns the `Header` corresponding to the pending state.
    pub fn pending_header(&self) -> Option<Header> {
        self.pending_sealed_header().map(|sealed_header| sealed_header.unseal())
    }

    /// Returns the `SealedBlock` corresponding to the pending state.
    pub fn pending_block(&self) -> Option<SealedBlock> {
        self.pending_state().map(|block_state| block_state.block().block().clone())
    }

    /// Returns the `SealedBlockWithSenders` corresponding to the pending state.
    pub fn pending_block_with_senders(&self) -> Option<SealedBlockWithSenders> {
        self.pending_state()
            .and_then(|block_state| block_state.block().block().clone().seal_with_senders())
    }

    /// Returns a tuple with the `SealedBlock` corresponding to the pending
    /// state and a vector of its `Receipt`s.
    pub fn pending_block_and_receipts(&self) -> Option<(SealedBlock, Vec<Receipt>)> {
        self.pending_state().map(|block_state| {
            (block_state.block().block().clone(), block_state.executed_block_receipts())
        })
    }

    /// Subscribe to new blocks events.
    pub fn subscribe_canon_state(&self) -> CanonStateNotifications {
        self.inner.canon_state_notification_sender.subscribe()
    }

    /// Attempts to send a new [`CanonStateNotification`] to all active Receiver handles.
    pub fn notify_canon_state(&self, event: CanonStateNotification) {
        self.inner.canon_state_notification_sender.send(event).ok();
    }
}

/// State after applying the given block, this block is part of the canonical chain that partially
/// stored in memory and can be traced back to a canonical block on disk.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct BlockState {
    /// The executed block that determines the state after this block has been executed.
    block: ExecutedBlock,
    /// The block's parent block if it exists.
    parent: Option<Box<BlockState>>,
}

#[allow(dead_code)]
impl BlockState {
    /// `BlockState` constructor.
    pub const fn new(block: ExecutedBlock) -> Self {
        Self { block, parent: None }
    }

    /// `BlockState` constructor with parent.
    pub fn with_parent(block: ExecutedBlock, parent: Option<Self>) -> Self {
        Self { block, parent: parent.map(Box::new) }
    }

    /// Returns the hash and block of the on disk block this state can be traced back to.
    pub fn anchor(&self) -> BlockNumHash {
        if let Some(parent) = &self.parent {
            parent.anchor()
        } else {
            self.block.block().parent_num_hash()
        }
    }

    /// Returns the executed block that determines the state.
    pub fn block(&self) -> ExecutedBlock {
        self.block.clone()
    }

    /// Returns the hash of executed block that determines the state.
    pub fn hash(&self) -> B256 {
        self.block.block().hash()
    }

    /// Returns the block number of executed block that determines the state.
    pub fn number(&self) -> u64 {
        self.block.block().number
    }

    /// Returns the state root after applying the executed block that determines
    /// the state.
    pub fn state_root(&self) -> B256 {
        self.block.block().header.state_root
    }

    /// Returns the `Receipts` of executed block that determines the state.
    pub fn receipts(&self) -> &Receipts {
        &self.block.execution_outcome().receipts
    }

    /// Returns a vector of `Receipt` of executed block that determines the state.
    /// We assume that the `Receipts` in the executed block `ExecutionOutcome`
    /// has only one element corresponding to the executed block associated to
    /// the state.
    pub fn executed_block_receipts(&self) -> Vec<Receipt> {
        let receipts = self.receipts();

        debug_assert!(
            receipts.receipt_vec.len() <= 1,
            "Expected at most one block's worth of receipts, found {}",
            receipts.receipt_vec.len()
        );

        receipts
            .receipt_vec
            .first()
            .map(|block_receipts| {
                block_receipts.iter().filter_map(|opt_receipt| opt_receipt.clone()).collect()
            })
            .unwrap_or_default()
    }
}

/// Represents an executed block stored in-memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutedBlock {
    /// Sealed block the rest of fields refer to.
    pub block: Arc<SealedBlock>,
    /// Block's senders.
    pub senders: Arc<Vec<Address>>,
    /// Block's execution outcome.
    pub execution_output: Arc<ExecutionOutcome>,
    /// Block's hashedst state.
    pub hashed_state: Arc<HashedPostState>,
    /// Trie updates that result of applying the block.
    pub trie: Arc<TrieUpdates>,
}

impl ExecutedBlock {
    /// `ExecutedBlock` constructor.
    pub const fn new(
        block: Arc<SealedBlock>,
        senders: Arc<Vec<Address>>,
        execution_output: Arc<ExecutionOutcome>,
        hashed_state: Arc<HashedPostState>,
        trie: Arc<TrieUpdates>,
    ) -> Self {
        Self { block, senders, execution_output, hashed_state, trie }
    }

    /// Returns a reference to the executed block.
    pub fn block(&self) -> &SealedBlock {
        &self.block
    }

    /// Returns a reference to the block's senders
    pub fn senders(&self) -> &Vec<Address> {
        &self.senders
    }

    /// Returns a [`SealedBlockWithSenders`]
    ///
    /// Note: this clones the block and senders.
    pub fn sealed_block_with_senders(&self) -> SealedBlockWithSenders {
        SealedBlockWithSenders { block: (*self.block).clone(), senders: (*self.senders).clone() }
    }

    /// Returns a reference to the block's execution outcome
    pub fn execution_outcome(&self) -> &ExecutionOutcome {
        &self.execution_output
    }

    /// Returns a reference to the hashed state result of the execution outcome
    pub fn hashed_state(&self) -> &HashedPostState {
        &self.hashed_state
    }

    /// Returns a reference to the trie updates for the block
    pub fn trie_updates(&self) -> &TrieUpdates {
        &self.trie
    }
}

/// Non-empty chain of blocks.
#[derive(Debug)]
pub enum NewCanonicalChain {
    /// A simple append to the current canonical head
    Commit {
        /// all blocks that lead back to the canonical head
        new: Vec<ExecutedBlock>,
    },
    /// A reorged chain consists of two chains that trace back to a shared ancestor block at which
    /// point they diverge.
    Reorg {
        /// All blocks of the _new_ chain
        new: Vec<ExecutedBlock>,
        /// All blocks of the _old_ chain
        old: Vec<ExecutedBlock>,
    },
}

impl NewCanonicalChain {
    /// Converts the new chain into a notification that will be emitted to listeners
    pub fn to_chain_notification(&self) -> CanonStateNotification {
        // TODO: do we need to merge execution outcome for multiblock commit or reorg?
        //  implement this properly
        match self {
            Self::Commit { new } => CanonStateNotification::Commit {
                new: Arc::new(Chain::new(
                    new.iter().map(ExecutedBlock::sealed_block_with_senders),
                    new.last().unwrap().execution_output.deref().clone(),
                    None,
                )),
            },
            Self::Reorg { new, old } => CanonStateNotification::Reorg {
                new: Arc::new(Chain::new(
                    new.iter().map(ExecutedBlock::sealed_block_with_senders),
                    new.last().unwrap().execution_output.deref().clone(),
                    None,
                )),
                old: Arc::new(Chain::new(
                    old.iter().map(ExecutedBlock::sealed_block_with_senders),
                    old.last().unwrap().execution_output.deref().clone(),
                    None,
                )),
            },
        }
    }

    /// Returns the new tip of the chain.
    ///
    /// Returns the new tip for [`Self::Reorg`] and [`Self::Commit`] variants which commit at least
    /// 1 new block.
    pub fn tip(&self) -> &SealedBlock {
        match self {
            Self::Commit { new } | Self::Reorg { new, .. } => {
                new.last().expect("non empty blocks").block()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{get_executed_block_with_number, get_executed_block_with_receipts};
    use rand::Rng;
    use reth_primitives::Receipt;

    fn create_mock_state(block_number: u64) -> BlockState {
        BlockState::new(get_executed_block_with_number(block_number))
    }

    #[test]
    fn test_in_memory_state_impl_state_by_hash() {
        let mut state_by_hash = HashMap::new();
        let number = rand::thread_rng().gen::<u64>();
        let state = Arc::new(create_mock_state(number));
        state_by_hash.insert(state.hash(), state.clone());

        let in_memory_state = InMemoryState::new(state_by_hash, HashMap::new(), None);

        assert_eq!(in_memory_state.state_by_hash(state.hash()), Some(state));
        assert_eq!(in_memory_state.state_by_hash(B256::random()), None);
    }

    #[test]
    fn test_in_memory_state_impl_state_by_number() {
        let mut state_by_hash = HashMap::new();
        let mut hash_by_number = HashMap::new();

        let number = rand::thread_rng().gen::<u64>();
        let state = Arc::new(create_mock_state(number));
        let hash = state.hash();

        state_by_hash.insert(hash, state.clone());
        hash_by_number.insert(number, hash);

        let in_memory_state = InMemoryState::new(state_by_hash, hash_by_number, None);

        assert_eq!(in_memory_state.state_by_number(number), Some(state));
        assert_eq!(in_memory_state.state_by_number(number + 1), None);
    }

    #[test]
    fn test_in_memory_state_impl_head_state() {
        let mut state_by_hash = HashMap::new();
        let mut hash_by_number = HashMap::new();
        let state1 = Arc::new(create_mock_state(1));
        let state2 = Arc::new(create_mock_state(2));
        let hash1 = state1.hash();
        let hash2 = state2.hash();
        hash_by_number.insert(1, hash1);
        hash_by_number.insert(2, hash2);
        state_by_hash.insert(hash1, state1);
        state_by_hash.insert(hash2, state2);

        let in_memory_state = InMemoryState::new(state_by_hash, hash_by_number, None);
        let head_state = in_memory_state.head_state().unwrap();

        assert_eq!(head_state.hash(), hash2);
        assert_eq!(head_state.number(), 2);
    }

    #[test]
    fn test_in_memory_state_impl_pending_state() {
        let pending_number = rand::thread_rng().gen::<u64>();
        let pending_state = create_mock_state(pending_number);
        let pending_hash = pending_state.hash();

        let in_memory_state =
            InMemoryState::new(HashMap::new(), HashMap::new(), Some(pending_state));

        let result = in_memory_state.pending_state();
        assert!(result.is_some());
        let actual_pending_state = result.unwrap();
        assert_eq!(actual_pending_state.block.block().hash(), pending_hash);
        assert_eq!(actual_pending_state.block.block().number, pending_number);
    }

    #[test]
    fn test_in_memory_state_impl_no_pending_state() {
        let in_memory_state = InMemoryState::new(HashMap::new(), HashMap::new(), None);

        assert_eq!(in_memory_state.pending_state(), None);
    }

    #[test]
    fn test_state_new() {
        let number = rand::thread_rng().gen::<u64>();
        let block = get_executed_block_with_number(number);

        let state = BlockState::new(block.clone());

        assert_eq!(state.block(), block);
    }

    #[test]
    fn test_state_block() {
        let number = rand::thread_rng().gen::<u64>();
        let block = get_executed_block_with_number(number);

        let state = BlockState::new(block.clone());

        assert_eq!(state.block(), block);
    }

    #[test]
    fn test_state_hash() {
        let number = rand::thread_rng().gen::<u64>();
        let block = get_executed_block_with_number(number);

        let state = BlockState::new(block.clone());

        assert_eq!(state.hash(), block.block().hash());
    }

    #[test]
    fn test_state_number() {
        let number = rand::thread_rng().gen::<u64>();
        let block = get_executed_block_with_number(number);

        let state = BlockState::new(block);

        assert_eq!(state.number(), number);
    }

    #[test]
    fn test_state_state_root() {
        let number = rand::thread_rng().gen::<u64>();
        let block = get_executed_block_with_number(number);

        let state = BlockState::new(block.clone());

        assert_eq!(state.state_root(), block.block().state_root);
    }

    #[test]
    fn test_state_receipts() {
        let receipts = Receipts { receipt_vec: vec![vec![Some(Receipt::default())]] };

        let block = get_executed_block_with_receipts(receipts.clone());

        let state = BlockState::new(block);

        assert_eq!(state.receipts(), &receipts);
    }

    #[test]
    fn test_in_memory_state_chain_update() {
        let state = CanonicalInMemoryState::new(HashMap::new(), HashMap::new(), None);
        let block1 = get_executed_block_with_number(0);
        let block2 = get_executed_block_with_number(0);
        let chain = NewCanonicalChain::Commit { new: vec![block1.clone()] };
        state.update_chain(chain);
        assert_eq!(state.head_state().unwrap().block().block().hash(), block1.block().hash());
        assert_eq!(state.state_by_number(0).unwrap().block().block().hash(), block1.block().hash());

        let chain = NewCanonicalChain::Reorg { new: vec![block2.clone()], old: vec![block1] };
        state.update_chain(chain);
        assert_eq!(state.head_state().unwrap().block().block().hash(), block2.block().hash());
        assert_eq!(state.state_by_number(0).unwrap().block().block().hash(), block2.block().hash());

        assert_eq!(state.inner.in_memory_state.block_count(), 1);
    }
}