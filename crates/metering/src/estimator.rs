use crate::{MeteredTransaction, MeteringCache};
use alloy_primitives::U256;
use parking_lot::RwLock;
use std::{collections::BTreeMap, sync::Arc};

/// Resources that influence flashblock inclusion ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceKind {
    GasUsed,
    ExecutionTime,
    StateRootTime,
    DataAvailability,
}

impl ResourceKind {
    fn use_it_or_lose_it(self) -> bool {
        matches!(self, ResourceKind::ExecutionTime)
    }
}

/// Amount of resources required by the bundle being priced.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceDemand {
    pub gas_used: Option<u64>,
    pub execution_time_us: Option<u128>,
    pub state_root_time_us: Option<u128>,
    pub data_availability_bytes: Option<u64>,
}

impl ResourceDemand {
    fn demand_for(&self, resource: ResourceKind) -> Option<u128> {
        match resource {
            ResourceKind::GasUsed => self.gas_used.map(|v| v as u128),
            ResourceKind::ExecutionTime => self.execution_time_us,
            ResourceKind::StateRootTime => self.state_root_time_us,
            ResourceKind::DataAvailability => self.data_availability_bytes.map(|v| v as u128),
        }
    }
}

/// Fee quote for a given resource.
#[derive(Debug, Clone)]
pub struct ResourceEstimate {
    pub resource: ResourceKind,
    pub threshold_priority_fee: U256,
    pub recommended_priority_fee: U256,
    pub cumulative_usage: u128,
    pub supporting_transactions: usize,
    pub total_transactions: usize,
}

/// Estimates for a specific flashblock index.
#[derive(Debug, Clone)]
pub struct FlashblockResourceEstimates {
    pub flashblock_index: u64,
    pub resource_estimates: BTreeMap<ResourceKind, ResourceEstimate>,
}

/// Aggregated estimates for a block.
#[derive(Debug, Clone)]
pub struct BlockPriorityEstimates {
    pub block_number: u64,
    pub flashblocks: Vec<FlashblockResourceEstimates>,
    pub next_block: BTreeMap<ResourceKind, ResourceEstimate>,
    pub next_flashblock: BTreeMap<ResourceKind, ResourceEstimate>,
}

/// Computes resource fee estimates based on cached flashblock metering data.
pub struct PriorityFeeEstimator {
    cache: Arc<RwLock<MeteringCache>>,
    percentile: f64,
}

impl PriorityFeeEstimator {
    /// Creates a new estimator referencing the shared metering cache. `percentile`
    /// determines which point of the fee distribution (among transactions already
    /// scheduled above the threshold) is returned as the recommendation.
    pub fn new(cache: Arc<RwLock<MeteringCache>>, percentile: f64) -> Self {
        Self { cache, percentile }
    }

    /// Returns fee estimates for the provided block. If `block_number` is `None`
    /// the most recent block in the cache is used.
    pub fn estimate_for_block(
        &self,
        block_number: Option<u64>,
        demand: ResourceDemand,
    ) -> Option<BlockPriorityEstimates> {
        let cache_guard = self.cache.read();
        let block_metrics = match block_number {
            Some(target) => cache_guard.block(target),
            None => cache_guard.blocks_desc().next(),
        }?;

        let block_number = block_metrics.block_number;

        // Materialise sorted transactions per flashblock so we can drop the lock before
        // running the estimation logic.
        let mut flashblock_transactions = Vec::new();
        for flashblock in block_metrics.flashblocks() {
            let sorted: Vec<MeteredTransaction> = flashblock
                .transactions_sorted_by_priority_fee()
                .into_iter()
                .cloned()
                .collect();
            if sorted.is_empty() {
                continue;
            }
            flashblock_transactions.push((flashblock.flashblock_index, sorted));
        }
        drop(cache_guard);

        if flashblock_transactions.is_empty() {
            return None;
        }

        // Flatten into a single list for use-it-or-lose-it resources.
        let mut aggregate_transactions: Vec<MeteredTransaction> = flashblock_transactions
            .iter()
            .flat_map(|(_, txs)| txs.iter().cloned())
            .collect();
        aggregate_transactions.sort_by(|a, b| a.priority_fee_per_gas.cmp(&b.priority_fee_per_gas));

        let mut flashblock_estimates = Vec::new();

        for (flashblock_index, txs) in flashblock_transactions {
            let mut resource_estimates = BTreeMap::new();
            for resource in [
                ResourceKind::GasUsed,
                ResourceKind::ExecutionTime,
                ResourceKind::StateRootTime,
                ResourceKind::DataAvailability,
            ] {
                let Some(demand_value) = demand.demand_for(resource) else {
                    continue;
                };

                let estimate = if resource.use_it_or_lose_it() {
                    compute_resource_estimate(
                        &aggregate_transactions,
                        demand_value,
                        usage_extractor(resource),
                        self.percentile,
                    )
                } else {
                    compute_resource_estimate(
                        &txs,
                        demand_value,
                        usage_extractor(resource),
                        self.percentile,
                    )
                };

                if let Some(mut est) = estimate {
                    est.resource = resource;
                    resource_estimates.insert(resource, est.clone());
                }
            }

            flashblock_estimates.push(FlashblockResourceEstimates {
                flashblock_index,
                resource_estimates,
            });
        }

        let (next_block, next_flashblock) = summarise_extrema(&flashblock_estimates);

        Some(BlockPriorityEstimates {
            block_number,
            flashblocks: flashblock_estimates,
            next_block,
            next_flashblock,
        })
    }
}

#[derive(Debug, Clone)]
struct ComputedEstimate {
    threshold_priority_fee: U256,
    recommended_priority_fee: U256,
    cumulative_usage: u128,
    supporting_transactions: usize,
    total_transactions: usize,
}

fn compute_resource_estimate(
    transactions: &[MeteredTransaction],
    demand: u128,
    usage_fn: fn(&MeteredTransaction) -> u128,
    percentile: f64,
) -> Option<ResourceEstimate> {
    if transactions.is_empty() || demand == 0 {
        return None;
    }

    let computed = compute_estimate_core(transactions, demand, usage_fn, percentile)?;
    Some(ResourceEstimate {
        resource: ResourceKind::GasUsed, // placeholder, caller rewrites.
        threshold_priority_fee: computed.threshold_priority_fee,
        recommended_priority_fee: computed.recommended_priority_fee,
        cumulative_usage: computed.cumulative_usage,
        supporting_transactions: computed.supporting_transactions,
        total_transactions: computed.total_transactions,
    })
}

fn compute_estimate_core(
    transactions: &[MeteredTransaction],
    demand: u128,
    usage_fn: fn(&MeteredTransaction) -> u128,
    percentile: f64,
) -> Option<ComputedEstimate> {
    if transactions.is_empty() {
        return None;
    }

    let mut cumulative = 0u128;
    let mut threshold_idx = None;
    for (idx, tx) in transactions.iter().enumerate() {
        cumulative = cumulative.saturating_add(usage_fn(tx));
        if cumulative >= demand {
            threshold_idx = Some(idx);
            break;
        }
    }

    let idx = threshold_idx?;
    let threshold_fee = transactions[idx].priority_fee_per_gas;
    let remaining = &transactions[idx + 1..];
    let percentile = percentile.clamp(0.0, 1.0);
    let recommended_fee = if remaining.is_empty() {
        threshold_fee
    } else {
        let pos = ((remaining.len() - 1) as f64 * percentile).round() as usize;
        remaining[pos.min(remaining.len() - 1)].priority_fee_per_gas
    };

    Some(ComputedEstimate {
        threshold_priority_fee: threshold_fee,
        recommended_priority_fee: recommended_fee,
        cumulative_usage: cumulative,
        supporting_transactions: idx + 1,
        total_transactions: transactions.len(),
    })
}

fn usage_extractor(resource: ResourceKind) -> fn(&MeteredTransaction) -> u128 {
    match resource {
        ResourceKind::GasUsed => |tx: &MeteredTransaction| tx.gas_used as u128,
        ResourceKind::ExecutionTime => |tx: &MeteredTransaction| tx.execution_time_us,
        ResourceKind::StateRootTime => |tx: &MeteredTransaction| tx.state_root_time_us,
        ResourceKind::DataAvailability => {
            |tx: &MeteredTransaction| tx.data_availability_bytes as u128
        }
    }
}

fn summarise_extrema(
    flashblocks: &[FlashblockResourceEstimates],
) -> (
    BTreeMap<ResourceKind, ResourceEstimate>,
    BTreeMap<ResourceKind, ResourceEstimate>,
) {
    let mut min_map: BTreeMap<ResourceKind, ResourceEstimate> = BTreeMap::new();
    let mut max_map: BTreeMap<ResourceKind, ResourceEstimate> = BTreeMap::new();

    for flashblock in flashblocks {
        for (resource, estimate) in &flashblock.resource_estimates {
            min_map
                .entry(*resource)
                .and_modify(|current| {
                    if estimate.recommended_priority_fee < current.recommended_priority_fee {
                        *current = estimate.clone();
                    }
                })
                .or_insert_with(|| estimate.clone());

            max_map
                .entry(*resource)
                .and_modify(|current| {
                    if estimate.recommended_priority_fee > current.recommended_priority_fee {
                        *current = estimate.clone();
                    }
                })
                .or_insert_with(|| estimate.clone());
        }
    }

    (min_map, max_map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    fn tx(priority: u64, usage: u64) -> MeteredTransaction {
        MeteredTransaction {
            tx_hash: Default::default(),
            priority_fee_per_gas: U256::from(priority),
            gas_used: usage,
            execution_time_us: usage as u128,
            state_root_time_us: usage as u128,
            data_availability_bytes: usage,
        }
    }

    #[test]
    fn compute_estimate_basic() {
        let txs = vec![tx(1, 10), tx(2, 10), tx(3, 10)];
        let quote = compute_estimate_core(&txs, 15, usage_extractor(ResourceKind::GasUsed), 0.5)
            .expect("quote");
        assert_eq!(quote.threshold_priority_fee, U256::from(2));
        assert_eq!(quote.recommended_priority_fee, U256::from(3));
        assert_eq!(quote.cumulative_usage, 20);
        assert_eq!(quote.supporting_transactions, 2);
    }
}
