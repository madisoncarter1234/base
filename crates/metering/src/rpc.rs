use alloy_consensus::Header;
use alloy_eips::{BlockNumberOrTag, Encodable2718};
use alloy_primitives::U256;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use op_alloy_flz::tx_estimated_size_fjord_bytes;
use reth::providers::BlockReaderIdExt;
use reth_optimism_chainspec::OpChainSpec;
use reth_provider::{ChainSpecProvider, StateProviderFactory};
use std::collections::BTreeMap;
use std::sync::Arc;
use tips_core::types::{Bundle, MeterBundleResponse, ParsedBundle};
use tracing::{debug, error, info};

use crate::{
    BlockPriorityEstimates, PriorityFeeEstimator, ResourceDemand, ResourceEstimate, ResourceKind,
    meter_bundle,
};

/// Default percentile applied when selecting a recommended priority fee among
/// transactions already scheduled above the inclusion threshold.
pub const DEFAULT_PRIORITY_FEE_PERCENTILE: f64 = 0.5;

/// Human-friendly representation of a resource fee quote.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceFeeEstimateResponse {
    pub resource: String,
    pub threshold_priority_fee: String,
    pub recommended_priority_fee: String,
    pub cumulative_usage: String,
    pub supporting_transactions: u64,
    pub total_transactions: u64,
}

/// Priority-fee estimates for a single flashblock index.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlashblockFeeEstimatesResponse {
    pub flashblock_index: u64,
    pub estimates: Vec<ResourceFeeEstimateResponse>,
}

/// Response payload for `base_meteredPriorityFeePerGas`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteredPriorityFeeResponse {
    #[serde(flatten)]
    pub meter_bundle: MeterBundleResponse,
    pub flashblock_estimates: Vec<FlashblockFeeEstimatesResponse>,
    pub next_block: Vec<ResourceFeeEstimateResponse>,
    pub next_flashblock: Vec<ResourceFeeEstimateResponse>,
}

/// RPC API for transaction metering.
#[rpc(server, namespace = "base")]
pub trait MeteringApi {
    /// Simulates and meters a bundle of transactions.
    #[method(name = "meterBundle")]
    async fn meter_bundle(&self, bundle: Bundle) -> RpcResult<MeterBundleResponse>;

    /// Estimates the priority fee necessary for a bundle to be included in recently observed
    /// flashblocks, considering multiple resource constraints.
    #[method(name = "meteredPriorityFeePerGas")]
    async fn metered_priority_fee_per_gas(
        &self,
        bundle: Bundle,
    ) -> RpcResult<MeteredPriorityFeeResponse>;
}

/// Implementation of the metering RPC API.
pub struct MeteringApiImpl<Provider> {
    provider: Provider,
    priority_fee_estimator: Arc<PriorityFeeEstimator>,
}

impl<Provider> MeteringApiImpl<Provider>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt<Header = Header>
        + Clone,
{
    /// Creates a new instance of the metering API backed by the given provider and estimator.
    pub fn new(provider: Provider, priority_fee_estimator: Arc<PriorityFeeEstimator>) -> Self {
        Self {
            provider,
            priority_fee_estimator,
        }
    }

    fn run_metering(
        &self,
        bundle: Bundle,
    ) -> Result<(MeterBundleResponse, ResourceDemand), ErrorObjectOwned> {
        info!(
            num_transactions = &bundle.txs.len(),
            block_number = &bundle.block_number,
            "Starting bundle metering"
        );

        let header = self
            .provider
            .sealed_header_by_number_or_tag(BlockNumberOrTag::Latest)
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("Failed to get latest header: {e}"),
                    None::<()>,
                )
            })?
            .ok_or_else(|| {
                ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    "Latest block not found".to_string(),
                    None::<()>,
                )
            })?;

        let parsed_bundle = ParsedBundle::try_from(bundle).map_err(|e| {
            ErrorObjectOwned::owned(
                ErrorCode::InvalidParams.code(),
                format!("Failed to parse bundle: {e}"),
                None::<()>,
            )
        })?;

        let da_usage: u64 = parsed_bundle
            .txs
            .iter()
            .map(|tx| tx_estimated_size_fjord_bytes(&tx.encoded_2718()))
            .sum();

        let state_provider = self
            .provider
            .state_by_block_hash(header.hash())
            .map_err(|e| {
                error!(error = %e, "Failed to get state provider");
                ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("Failed to get state provider: {e}"),
                    None::<()>,
                )
            })?;

        let chain_spec = self.provider.chain_spec().clone();

        let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) =
            meter_bundle(state_provider, chain_spec, parsed_bundle, &header).map_err(|e| {
                error!(error = %e, "Bundle metering failed");
                ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("Bundle metering failed: {e}"),
                    None::<()>,
                )
            })?;

        let bundle_gas_price = if total_gas_used > 0 {
            (total_gas_fees / U256::from(total_gas_used)).to_string()
        } else {
            "0".to_string()
        };

        info!(
            bundle_hash = %bundle_hash,
            num_transactions = results.len(),
            total_gas_used = total_gas_used,
            total_execution_time_us = total_execution_time,
            "Bundle metering completed successfully"
        );

        let response = MeterBundleResponse {
            bundle_gas_price,
            bundle_hash,
            coinbase_diff: total_gas_fees.to_string(),
            eth_sent_to_coinbase: "0".to_string(),
            gas_fees: total_gas_fees.to_string(),
            results,
            state_block_number: header.number,
            state_flashblock_index: None,
            total_gas_used,
            total_execution_time_us: total_execution_time,
        };

        let resource_demand = ResourceDemand {
            gas_used: Some(total_gas_used),
            execution_time_us: Some(total_execution_time),
            state_root_time_us: None, // Populated when state-root metrics become available.
            data_availability_bytes: Some(da_usage),
        };

        Ok((response, resource_demand))
    }

    fn build_priority_fee_response(
        meter_bundle: MeterBundleResponse,
        estimates: BlockPriorityEstimates,
    ) -> MeteredPriorityFeeResponse {
        let flashblock_estimates = estimates
            .flashblocks
            .into_iter()
            .map(|flashblock| FlashblockFeeEstimatesResponse {
                flashblock_index: flashblock.flashblock_index,
                estimates: ordered_resource_estimates(flashblock.resource_estimates),
            })
            .collect();

        let next_block = ordered_resource_estimates(estimates.next_block);
        let next_flashblock = ordered_resource_estimates(estimates.next_flashblock);

        MeteredPriorityFeeResponse {
            meter_bundle,
            flashblock_estimates,
            next_block,
            next_flashblock,
        }
    }
}

#[async_trait]
impl<Provider> MeteringApiServer for MeteringApiImpl<Provider>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    async fn meter_bundle(&self, bundle: Bundle) -> RpcResult<MeterBundleResponse> {
        let (response, _) = self.run_metering(bundle)?;
        Ok(response)
    }

    async fn metered_priority_fee_per_gas(
        &self,
        bundle: Bundle,
    ) -> RpcResult<MeteredPriorityFeeResponse> {
        let (meter_bundle, resource_demand) = self.run_metering(bundle)?;

        debug!(?resource_demand, "Computing priority fee estimates");

        let estimates = self
            .priority_fee_estimator
            .estimate_for_block(None, resource_demand)
            .ok_or_else(|| {
                ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    "Priority fee data unavailable".to_string(),
                    None::<()>,
                )
            })?;

        let response = Self::build_priority_fee_response(meter_bundle, estimates);
        Ok(response)
    }
}

fn ordered_resource_estimates(
    estimates: BTreeMap<ResourceKind, ResourceEstimate>,
) -> Vec<ResourceFeeEstimateResponse> {
    let mut entries: Vec<_> = estimates.into_iter().collect();
    entries.sort_by_key(|(kind, _)| *kind);
    entries
        .into_iter()
        .map(|(resource, estimate)| to_response(resource, estimate))
        .collect()
}

fn to_response(resource: ResourceKind, estimate: ResourceEstimate) -> ResourceFeeEstimateResponse {
    ResourceFeeEstimateResponse {
        resource: resource.as_camel_case().to_string(),
        threshold_priority_fee: estimate.threshold_priority_fee.to_string(),
        recommended_priority_fee: estimate.recommended_priority_fee.to_string(),
        cumulative_usage: estimate.cumulative_usage.to_string(),
        supporting_transactions: estimate
            .supporting_transactions
            .try_into()
            .unwrap_or(u64::MAX),
        total_transactions: estimate.total_transactions.try_into().unwrap_or(u64::MAX),
    }
}

impl ResourceKind {
    fn as_camel_case(&self) -> &'static str {
        match self {
            ResourceKind::GasUsed => "gasUsed",
            ResourceKind::ExecutionTime => "executionTime",
            ResourceKind::StateRootTime => "stateRootTime",
            ResourceKind::DataAvailability => "dataAvailability",
        }
    }
}
