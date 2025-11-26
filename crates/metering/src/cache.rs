use alloy_primitives::B256;
use indexmap::IndexMap;
use std::collections::{BTreeMap, HashMap, VecDeque};

use alloy_primitives::U256;

/// Maximum number of flashblocks we expect per block.
const DEFAULT_FLASHBLOCKS_PER_BLOCK: usize = 10;

#[derive(Debug, Clone)]
pub struct MeteredTransaction {
    pub tx_hash: B256,
    pub priority_fee_per_gas: U256,
    pub gas_used: u64,
    pub execution_time_us: u128,
    pub state_root_time_us: u128,
    pub data_availability_bytes: u64,
}

impl MeteredTransaction {
    pub fn zeroed(tx_hash: B256) -> Self {
        Self {
            tx_hash,
            priority_fee_per_gas: U256::ZERO,
            gas_used: 0,
            execution_time_us: 0,
            state_root_time_us: 0,
            data_availability_bytes: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceTotals {
    pub gas_used: u64,
    pub execution_time_us: u128,
    pub state_root_time_us: u128,
    pub data_availability_bytes: u64,
}

impl ResourceTotals {
    fn accumulate(&mut self, tx: &MeteredTransaction) {
        self.gas_used = self.gas_used.saturating_add(tx.gas_used);
        self.execution_time_us = self.execution_time_us.saturating_add(tx.execution_time_us);
        self.state_root_time_us = self
            .state_root_time_us
            .saturating_add(tx.state_root_time_us);
        self.data_availability_bytes = self
            .data_availability_bytes
            .saturating_add(tx.data_availability_bytes);
    }

    fn subtract(&mut self, tx: &MeteredTransaction) {
        self.gas_used = self.gas_used.saturating_sub(tx.gas_used);
        self.execution_time_us = self.execution_time_us.saturating_sub(tx.execution_time_us);
        self.state_root_time_us = self
            .state_root_time_us
            .saturating_sub(tx.state_root_time_us);
        self.data_availability_bytes = self
            .data_availability_bytes
            .saturating_sub(tx.data_availability_bytes);
    }
}

/// Metrics for a single flashblock within a block.
#[derive(Debug)]
pub struct FlashblockMetrics {
    pub block_number: u64,
    pub flashblock_index: u64,
    /// Transactions keyed by hash in insertion order.
    transactions: IndexMap<B256, MeteredTransaction>,
    totals: ResourceTotals,
}

impl FlashblockMetrics {
    pub fn new(block_number: u64, flashblock_index: u64) -> Self {
        Self {
            block_number,
            flashblock_index,
            transactions: IndexMap::new(),
            totals: ResourceTotals::default(),
        }
    }

    pub fn upsert_transaction(&mut self, tx: MeteredTransaction) {
        let tx_hash = tx.tx_hash;
        if let Some(existing) = self.transactions.get(&tx_hash) {
            self.totals.subtract(existing);
        }
        self.totals.accumulate(&tx);
        self.transactions.insert(tx_hash, tx);
    }

    pub fn remove_transaction(&mut self, tx_hash: &B256) -> Option<MeteredTransaction> {
        let removed = self.transactions.shift_remove(tx_hash);
        if let Some(ref tx) = removed {
            self.totals.subtract(tx);
        }
        removed
    }

    pub fn totals(&self) -> ResourceTotals {
        self.totals
    }

    pub fn transactions(&self) -> impl Iterator<Item = &MeteredTransaction> {
        self.transactions.values()
    }

    pub fn transactions_sorted_by_priority_fee(&self) -> Vec<&MeteredTransaction> {
        let mut txs: Vec<&MeteredTransaction> = self.transactions.values().collect();
        txs.sort_by(|a, b| a.priority_fee_per_gas.cmp(&b.priority_fee_per_gas));
        txs
    }

    pub fn len(&self) -> usize {
        self.transactions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }
}

/// Aggregated metrics for a block, including per-flashblock breakdown.
#[derive(Debug)]
pub struct BlockMetrics {
    pub block_number: u64,
    flashblocks: BTreeMap<u64, FlashblockMetrics>,
    totals: ResourceTotals,
}

impl BlockMetrics {
    pub fn new(block_number: u64) -> Self {
        Self {
            block_number,
            flashblocks: BTreeMap::new(),
            totals: ResourceTotals::default(),
        }
    }

    pub fn flashblock_count(&self) -> usize {
        self.flashblocks.len()
    }

    pub fn flashblocks(&self) -> impl Iterator<Item = &FlashblockMetrics> {
        self.flashblocks.values()
    }

    pub fn flashblock(&self, flashblock_index: u64) -> Option<&FlashblockMetrics> {
        self.flashblocks.get(&flashblock_index)
    }

    pub fn flashblock_mut(&mut self, flashblock_index: u64) -> (&mut FlashblockMetrics, bool) {
        let is_new = !self.flashblocks.contains_key(&flashblock_index);
        let entry = self
            .flashblocks
            .entry(flashblock_index)
            .or_insert_with(|| FlashblockMetrics::new(self.block_number, flashblock_index));
        (entry, is_new)
    }

    pub fn totals(&self) -> ResourceTotals {
        self.totals
    }

    fn recompute_totals(&mut self) {
        self.totals = ResourceTotals::default();
        for flashblock in self.flashblocks.values() {
            let totals = flashblock.totals();
            self.totals.gas_used = self.totals.gas_used.saturating_add(totals.gas_used);
            self.totals.execution_time_us = self
                .totals
                .execution_time_us
                .saturating_add(totals.execution_time_us);
            self.totals.state_root_time_us = self
                .totals
                .state_root_time_us
                .saturating_add(totals.state_root_time_us);
            self.totals.data_availability_bytes = self
                .totals
                .data_availability_bytes
                .saturating_add(totals.data_availability_bytes);
        }
    }
}

/// In-memory cache maintaining metering data for the most recent blocks.
#[derive(Debug)]
pub struct MeteringCache {
    max_blocks: usize,
    blocks: VecDeque<BlockMetrics>,
    block_index: HashMap<u64, usize>,
    default_flashblocks_per_block: usize,
}

impl MeteringCache {
    /// Creates a new cache retaining at most `max_blocks` recent blocks.
    pub fn new(max_blocks: usize) -> Self {
        Self {
            max_blocks,
            blocks: VecDeque::new(),
            block_index: HashMap::new(),
            default_flashblocks_per_block: DEFAULT_FLASHBLOCKS_PER_BLOCK,
        }
    }

    pub fn max_blocks(&self) -> usize {
        self.max_blocks
    }

    pub fn set_default_flashblocks_per_block(&mut self, cap: usize) {
        self.default_flashblocks_per_block = cap;
    }

    pub fn block(&self, block_number: u64) -> Option<&BlockMetrics> {
        self.block_index
            .get(&block_number)
            .and_then(|&idx| self.blocks.get(idx))
    }

    pub fn block_mut(&mut self, block_number: u64) -> &mut BlockMetrics {
        if let Some(&idx) = self.block_index.get(&block_number) {
            return self.blocks.get_mut(idx).expect("block index out of bounds");
        }

        let block = BlockMetrics::new(block_number);
        self.blocks.push_back(block);
        let idx = self.blocks.len() - 1;
        self.block_index.insert(block_number, idx);

        self.evict_if_needed();
        self.blocks
            .get_mut(*self.block_index.get(&block_number).unwrap())
            .unwrap()
    }

    pub fn flashblock(
        &self,
        block_number: u64,
        flashblock_index: u64,
    ) -> Option<&FlashblockMetrics> {
        self.block(block_number)
            .and_then(|block| block.flashblock(flashblock_index))
    }

    pub fn upsert_transaction(
        &mut self,
        block_number: u64,
        flashblock_index: u64,
        tx: MeteredTransaction,
    ) {
        let flashblock_cap = self.default_flashblocks_per_block;
        let block = self.block_mut(block_number);
        let (flashblock, is_new_flashblock) = block.flashblock_mut(flashblock_index);
        flashblock.upsert_transaction(tx);
        if is_new_flashblock && block.flashblock_count() > flashblock_cap {
            tracing::warn!(
                target: "metering::cache",
                block_number,
                flashblock_index,
                count = block.flashblock_count(),
                "Block exceeded expected flashblock count"
            );
        }
        block.recompute_totals();
    }

    pub fn remove_transaction(
        &mut self,
        block_number: u64,
        flashblock_index: u64,
        tx_hash: &B256,
    ) -> Option<MeteredTransaction> {
        let block = self.block_mut(block_number);
        let (flashblock, _) = block.flashblock_mut(flashblock_index);
        let removed = flashblock.remove_transaction(tx_hash);
        block.recompute_totals();
        removed
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn blocks_desc(&self) -> impl Iterator<Item = &BlockMetrics> {
        self.blocks.iter().rev()
    }

    fn evict_if_needed(&mut self) {
        while self.blocks.len() > self.max_blocks {
            if let Some(oldest) = self.blocks.pop_front() {
                self.block_index.remove(&oldest.block_number);
            }

            // Rebuild index to maintain correctness.
            self.rebuild_index();
        }
    }

    fn rebuild_index(&mut self) {
        self.block_index.clear();
        for (idx, block) in self.blocks.iter().enumerate() {
            self.block_index.insert(block.block_number, idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tx(hash: u64, priority: u64) -> MeteredTransaction {
        let mut hash_bytes = [0u8; 32];
        hash_bytes[24..].copy_from_slice(&hash.to_be_bytes());
        MeteredTransaction {
            tx_hash: B256::new(hash_bytes),
            priority_fee_per_gas: U256::from(priority),
            gas_used: 10,
            execution_time_us: 5,
            state_root_time_us: 7,
            data_availability_bytes: 20,
        }
    }

    #[test]
    fn insert_and_retrieve_transactions() {
        let mut cache = MeteringCache::new(12);
        let tx1 = test_tx(1, 2);
        cache.upsert_transaction(100, 0, tx1.clone());

        let block = cache.block(100).unwrap();
        let flashblock = block.flashblocks().next().unwrap();
        assert_eq!(flashblock.len(), 1);
        assert_eq!(
            flashblock.transactions().next().unwrap().tx_hash,
            tx1.tx_hash
        );
    }

    #[test]
    fn replaces_existing_transaction() {
        let mut cache = MeteringCache::new(12);
        let mut tx1 = test_tx(1, 2);
        cache.upsert_transaction(100, 0, tx1.clone());
        tx1.gas_used = 42;
        cache.upsert_transaction(100, 0, tx1.clone());

        let block = cache.block(100).unwrap();
        let flashblock = block.flashblocks().next().unwrap();
        assert_eq!(flashblock.len(), 1);
        assert_eq!(
            flashblock.transactions().next().unwrap().gas_used,
            tx1.gas_used
        );
    }

    #[test]
    fn evicts_old_blocks() {
        let mut cache = MeteringCache::new(2);
        for block_number in 0..3u64 {
            cache.upsert_transaction(block_number, 0, test_tx(block_number, block_number));
        }
        assert!(cache.block(0).is_none());
        assert!(cache.block(1).is_some());
        assert!(cache.block(2).is_some());
    }

    #[test]
    fn transactions_sorted_by_priority_fee() {
        let mut cache = MeteringCache::new(12);
        cache.upsert_transaction(100, 0, test_tx(1, 30));
        cache.upsert_transaction(100, 0, test_tx(2, 10));
        cache.upsert_transaction(100, 0, test_tx(3, 20));

        let block = cache.block(100).unwrap();
        let flashblock = block.flashblocks().next().unwrap();
        let sorted: Vec<_> = flashblock
            .transactions_sorted_by_priority_fee()
            .iter()
            .map(|tx| tx.priority_fee_per_gas)
            .collect();
        assert_eq!(
            sorted,
            vec![U256::from(10u64), U256::from(20u64), U256::from(30u64)]
        );
    }
}
