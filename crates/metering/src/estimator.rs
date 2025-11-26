use crate::{MeteredTransaction, MeteringCache};
use alloy_primitives::U256;
use parking_lot::RwLock;
use std::sync::Arc;

/// Resources that influence flashblock inclusion ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceKind {
    GasUsed,
    ExecutionTime,
    StateRootTime,
    DataAvailability,
}

impl ResourceKind {
    /// Returns all resource kinds in a fixed order.
    pub fn all() -> [ResourceKind; 4] {
        [
            ResourceKind::GasUsed,
            ResourceKind::ExecutionTime,
            ResourceKind::StateRootTime,
            ResourceKind::DataAvailability,
        ]
    }

    /// Returns `true` if this resource is "use-it-or-lose-it", meaning capacity
    /// that isn't consumed in one flashblock cannot be reclaimed in later ones.
    ///
    /// Execution time is the canonical example: the block builder has a fixed
    /// time budget per block, and unused time in flashblock 0 doesn't roll over
    /// to flashblock 1. For these resources, the estimator aggregates usage
    /// across all flashblocks rather than evaluating each flashblock in isolation.
    ///
    /// Other resources like gas and DA bytes are bounded per-block but are
    /// evaluated per-flashblock since their limits apply independently.
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

/// Fee estimate for a single resource type.
///
/// The estimation algorithm answers: "What priority fee would my bundle need to pay
/// to displace enough lower-paying transactions to free up the resources I need?"
#[derive(Debug, Clone)]
pub struct ResourceEstimate {
    /// Minimum fee to displace enough capacity for the bundle's resource demand.
    pub threshold_priority_fee: U256,
    /// Recommended fee based on a percentile of transactions above the threshold.
    /// Provides a safety margin over the bare minimum.
    pub recommended_priority_fee: U256,
    /// Total resource usage of transactions at or below the threshold.
    pub cumulative_usage: u128,
    /// Number of transactions that would be displaced at the threshold fee.
    pub supporting_transactions: usize,
    /// Total transactions considered in the estimate.
    pub total_transactions: usize,
}

/// Per-resource fee estimates.
///
/// Each field corresponds to a resource type. `None` indicates the resource
/// was not requested or could not be estimated (e.g., demand exceeds capacity).
#[derive(Debug, Clone, Default)]
pub struct ResourceEstimates {
    pub gas_used: Option<ResourceEstimate>,
    pub execution_time: Option<ResourceEstimate>,
    pub state_root_time: Option<ResourceEstimate>,
    pub data_availability: Option<ResourceEstimate>,
}

impl ResourceEstimates {
    /// Returns the estimate for the given resource kind.
    pub fn get(&self, kind: ResourceKind) -> Option<&ResourceEstimate> {
        match kind {
            ResourceKind::GasUsed => self.gas_used.as_ref(),
            ResourceKind::ExecutionTime => self.execution_time.as_ref(),
            ResourceKind::StateRootTime => self.state_root_time.as_ref(),
            ResourceKind::DataAvailability => self.data_availability.as_ref(),
        }
    }

    /// Sets the estimate for the given resource kind.
    pub fn set(&mut self, kind: ResourceKind, estimate: ResourceEstimate) {
        match kind {
            ResourceKind::GasUsed => self.gas_used = Some(estimate),
            ResourceKind::ExecutionTime => self.execution_time = Some(estimate),
            ResourceKind::StateRootTime => self.state_root_time = Some(estimate),
            ResourceKind::DataAvailability => self.data_availability = Some(estimate),
        }
    }

    /// Iterates over all present estimates with their resource kind.
    pub fn iter(&self) -> impl Iterator<Item = (ResourceKind, &ResourceEstimate)> {
        [
            (ResourceKind::GasUsed, &self.gas_used),
            (ResourceKind::ExecutionTime, &self.execution_time),
            (ResourceKind::StateRootTime, &self.state_root_time),
            (ResourceKind::DataAvailability, &self.data_availability),
        ]
        .into_iter()
        .filter_map(|(kind, opt)| opt.as_ref().map(|est| (kind, est)))
    }

    /// Returns true if no estimates are present.
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }
}

/// Estimates for a specific flashblock index.
#[derive(Debug, Clone)]
pub struct FlashblockResourceEstimates {
    pub flashblock_index: u64,
    pub estimates: ResourceEstimates,
}

/// Aggregated estimates for a block.
#[derive(Debug, Clone)]
pub struct BlockPriorityEstimates {
    pub block_number: u64,
    pub flashblocks: Vec<FlashblockResourceEstimates>,
    /// Minimum recommended fee across all flashblocks (easiest inclusion).
    pub min_across_flashblocks: ResourceEstimates,
    /// Maximum recommended fee across all flashblocks (most competitive).
    pub max_across_flashblocks: ResourceEstimates,
}

/// Rolling estimates aggregated across multiple recent blocks.
#[derive(Debug, Clone)]
pub struct RollingPriorityEstimates {
    /// Number of blocks that contributed to this estimate.
    pub blocks_sampled: usize,
    /// Per-resource estimates (median across sampled blocks).
    pub estimates: ResourceEstimates,
    /// Single recommended fee: maximum across all resources.
    pub recommended_priority_fee: U256,
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
    ///
    /// Returns `None` if the cache is empty, the requested block is not cached,
    /// or no transactions exist in the cached flashblocks.
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
            let mut estimates = ResourceEstimates::default();
            for resource in ResourceKind::all() {
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

                if let Some(est) = estimate {
                    estimates.set(resource, est);
                }
            }

            flashblock_estimates.push(FlashblockResourceEstimates {
                flashblock_index,
                estimates,
            });
        }

        let (min_across_flashblocks, max_across_flashblocks) =
            compute_min_max_estimates(&flashblock_estimates);

        Some(BlockPriorityEstimates {
            block_number,
            flashblocks: flashblock_estimates,
            min_across_flashblocks,
            max_across_flashblocks,
        })
    }

    /// Returns rolling fee estimates aggregated across the most recent blocks in the cache.
    ///
    /// For each resource, computes estimates per-block and takes the median recommended fee.
    /// The final `recommended_priority_fee` is the maximum across all resources.
    ///
    /// Returns `None` if the cache is empty or no blocks contain transaction data.
    pub fn estimate_rolling(&self, demand: ResourceDemand) -> Option<RollingPriorityEstimates> {
        let cache_guard = self.cache.read();
        let block_numbers: Vec<u64> = cache_guard
            .blocks_desc()
            .map(|b| b.block_number)
            .collect();
        drop(cache_guard);

        if block_numbers.is_empty() {
            return None;
        }

        // Collect per-block max estimates.
        let block_estimates: Vec<ResourceEstimates> = block_numbers
            .iter()
            .filter_map(|&n| self.estimate_for_block(Some(n), demand))
            .map(|e| e.max_across_flashblocks)
            .collect();

        if block_estimates.is_empty() {
            return None;
        }

        // Compute median fee for each resource across blocks.
        let mut estimates = ResourceEstimates::default();
        let mut max_fee = U256::ZERO;

        for resource in ResourceKind::all() {
            let mut fees: Vec<U256> = block_estimates
                .iter()
                .filter_map(|e| e.get(resource))
                .map(|e| e.recommended_priority_fee)
                .collect();

            if fees.is_empty() {
                continue;
            }

            fees.sort();
            let median_fee = fees[fees.len() / 2];
            max_fee = max_fee.max(median_fee);

            estimates.set(
                resource,
                ResourceEstimate {
                    threshold_priority_fee: median_fee,
                    recommended_priority_fee: median_fee,
                    cumulative_usage: 0,
                    supporting_transactions: 0,
                    total_transactions: 0,
                },
            );
        }

        if estimates.is_empty() {
            return None;
        }

        Some(RollingPriorityEstimates {
            blocks_sampled: block_numbers.len(),
            estimates,
            recommended_priority_fee: max_fee,
        })
    }
}

/// Internal result from the core estimation algorithm.
#[derive(Debug, Clone)]
struct ComputedEstimate {
    threshold_priority_fee: U256,
    recommended_priority_fee: U256,
    cumulative_usage: u128,
    supporting_transactions: usize,
    total_transactions: usize,
}

/// Computes a fee estimate for a single resource type.
///
/// Thin wrapper around [`compute_estimate_core`] that converts the result
/// to a [`ResourceEstimate`].
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
        threshold_priority_fee: computed.threshold_priority_fee,
        recommended_priority_fee: computed.recommended_priority_fee,
        cumulative_usage: computed.cumulative_usage,
        supporting_transactions: computed.supporting_transactions,
        total_transactions: computed.total_transactions,
    })
}

/// Core estimation algorithm.
///
/// Given a list of transactions sorted by priority fee (ascending) and a resource
/// demand, determines the minimum priority fee needed to "displace" enough existing
/// transactions to free up the required capacity.
///
/// # Algorithm
///
/// 1. Walk transactions from lowest to highest priority fee, accumulating resource usage.
/// 2. Stop when cumulative usage >= demand. This transaction's fee is the **threshold**:
///    paying at least this much would displace all lower-fee transactions.
/// 3. The **recommended** fee is chosen from transactions above the threshold at the
///    given percentile, providing a safety margin.
///
/// # Example
///
/// ```text
/// Transactions (sorted by priority_fee ascending):
///   tx0: priority=1, gas=10
///   tx1: priority=2, gas=10
///   tx2: priority=3, gas=10
///
/// Demand: 15 gas
///
/// Walk:
///   cumulative=10 after tx0 (not enough)
///   cumulative=20 after tx1 (>= 15, threshold found)
///
/// threshold_fee = 2 (tx1's fee)
/// remaining = [tx2]
/// recommended_fee = 3 (50th percentile of remaining)
/// ```
fn compute_estimate_core(
    transactions: &[MeteredTransaction],
    demand: u128,
    usage_fn: fn(&MeteredTransaction) -> u128,
    percentile: f64,
) -> Option<ComputedEstimate> {
    if transactions.is_empty() {
        return None;
    }

    // Walk transactions low-to-high, accumulating resource usage until we have
    // enough capacity to satisfy the bundle's demand.
    let mut cumulative = 0u128;
    let mut threshold_idx = None;
    for (idx, tx) in transactions.iter().enumerate() {
        cumulative = cumulative.saturating_add(usage_fn(tx));
        if cumulative >= demand {
            threshold_idx = Some(idx);
            break;
        }
    }

    // If we never accumulated enough, the bundle's demand exceeds total capacity.
    let idx = threshold_idx?;

    // The threshold fee is the priority fee of the transaction where we crossed
    // the demand threshold. Paying this much displaces all transactions below.
    let threshold_fee = transactions[idx].priority_fee_per_gas;

    // For the recommended fee, look at transactions above the threshold and pick
    // one at the specified percentile. This provides a buffer above the minimum.
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

/// Returns a function that extracts the relevant resource usage from a transaction.
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

/// Computes the minimum and maximum recommended fees across all flashblocks.
///
/// Returns two `ResourceEstimates`:
/// - First: For each resource, the estimate with the lowest recommended fee (easiest inclusion).
/// - Second: For each resource, the estimate with the highest recommended fee (most competitive).
fn compute_min_max_estimates(
    flashblocks: &[FlashblockResourceEstimates],
) -> (ResourceEstimates, ResourceEstimates) {
    let mut min_estimates = ResourceEstimates::default();
    let mut max_estimates = ResourceEstimates::default();

    for flashblock in flashblocks {
        for (resource, estimate) in flashblock.estimates.iter() {
            // Update min.
            let current_min = min_estimates.get(resource);
            if current_min.is_none()
                || estimate.recommended_priority_fee
                    < current_min.unwrap().recommended_priority_fee
            {
                min_estimates.set(resource, estimate.clone());
            }

            // Update max.
            let current_max = max_estimates.get(resource);
            if current_max.is_none()
                || estimate.recommended_priority_fee
                    > current_max.unwrap().recommended_priority_fee
            {
                max_estimates.set(resource, estimate.clone());
            }
        }
    }

    (min_estimates, max_estimates)
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
