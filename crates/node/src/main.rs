use alloy_primitives::{TxHash, keccak256};
use base_reth_flashblocks_rpc::rpc::{EthApiExt, EthApiOverrideServer};
use base_reth_flashblocks_rpc::state::FlashblocksState;
use base_reth_flashblocks_rpc::subscription::{Flashblock, FlashblocksSubscriber};
use base_reth_metering::{
    FlashblockInclusion, KafkaBundleConsumer, KafkaBundleConsumerConfig, MeteredTransaction,
    MeteringApiImpl, MeteringApiServer, MeteringCache, PriorityFeeEstimator, ResourceAnnotator,
    ResourceLimits,
};

/// Default percentile applied when selecting a recommended priority fee among
/// transactions already scheduled above the inclusion threshold.
const DEFAULT_PRIORITY_FEE_PERCENTILE: f64 = 0.5;

/// Default gas limit per flashblock (30M gas).
const DEFAULT_GAS_LIMIT: u64 = 30_000_000;

/// Default execution time budget per block in microseconds (50ms).
const DEFAULT_EXECUTION_TIME_US: u128 = 50_000;

/// Default data availability limit per flashblock in bytes (120KB).
const DEFAULT_DA_BYTES: u64 = 120_000;

/// Default state root time budget per block in microseconds (disabled by default).
const DEFAULT_STATE_ROOT_TIME_US: Option<u128> = None;

/// Default priority fee returned when a resource is not congested (1 wei).
const DEFAULT_UNCONGESTED_PRIORITY_FEE: u64 = 1;

/// Default number of recent blocks retained in the metering cache.
const DEFAULT_METERING_CACHE_SIZE: usize = 12;

use base_reth_transaction_tracing::transaction_tracing_exex;
use clap::Parser;
use eyre::bail;
use futures_util::TryStreamExt;
use once_cell::sync::OnceCell;
use parking_lot::RwLock;
use rdkafka::config::ClientConfig;
use reth::builder::{Node, NodeHandle};
use reth::version::{
    RethCliVersionConsts, default_reth_version_metadata, try_init_version_metadata,
};
use reth::{
    builder::{EngineNodeLauncher, TreeConfig},
    providers::providers::BlockchainProvider,
};
use reth_exex::ExExEvent;
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
use reth_optimism_node::OpNode;
use reth_optimism_node::args::RollupArgs;
use std::sync::Arc;
use tips_core::kafka::load_kafka_config_from_file;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use url::Url;

pub const NODE_RETH_CLIENT_VERSION: &str = concat!("base/v", env!("CARGO_PKG_VERSION"));

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

#[derive(Debug, Clone, PartialEq, clap::Args)]
#[command(next_help_heading = "Rollup")]
struct Args {
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    #[arg(long = "websocket-url", value_name = "WEBSOCKET_URL")]
    pub websocket_url: Option<String>,

    /// Enable transaction tracing ExEx for mempool-to-block timing analysis
    #[arg(
        long = "enable-transaction-tracing",
        value_name = "ENABLE_TRANSACTION_TRACING"
    )]
    pub enable_transaction_tracing: bool,

    /// Enable `info` logs for transaction tracing
    #[arg(
        long = "enable-transaction-tracing-logs",
        value_name = "ENABLE_TRANSACTION_TRACING_LOGS"
    )]
    pub enable_transaction_tracing_logs: bool,

    /// Enable metering RPC for transaction bundle simulation
    #[arg(long = "enable-metering", value_name = "ENABLE_METERING")]
    pub enable_metering: bool,

    /// Kafka brokers (comma-separated) publishing accepted bundle events for metering.
    #[arg(long = "metering-kafka-brokers")]
    pub metering_kafka_brokers: Option<String>,

    /// Kafka topic carrying accepted bundle events.
    #[arg(long = "metering-kafka-topic")]
    pub metering_kafka_topic: Option<String>,

    /// Kafka consumer group id for metering ingestion.
    #[arg(long = "metering-kafka-group-id")]
    pub metering_kafka_group_id: Option<String>,

    /// Optional Kafka properties file (key=value per line) merged into the consumer config.
    #[arg(long = "metering-kafka-properties-file")]
    pub metering_kafka_properties_file: Option<String>,

    /// Gas limit per flashblock used for priority fee estimation.
    #[arg(long = "metering-gas-limit", default_value_t = DEFAULT_GAS_LIMIT)]
    pub metering_gas_limit: u64,

    /// Execution time budget per block in microseconds for priority fee estimation.
    #[arg(long = "metering-execution-time-us", default_value_t = DEFAULT_EXECUTION_TIME_US)]
    pub metering_execution_time_us: u128,

    /// State root time budget per block in microseconds for priority fee estimation.
    /// Not used until per-transaction state-root timing is available in the TIPS Kafka schema.
    #[arg(long = "metering-state-root-time-us")]
    pub metering_state_root_time_us: Option<u128>,

    /// Data availability limit per flashblock in bytes for priority fee estimation.
    #[arg(long = "metering-da-bytes", default_value_t = DEFAULT_DA_BYTES)]
    pub metering_da_bytes: u64,

    /// Percentile (0.0-1.0) for selecting recommended priority fee among transactions
    /// above the inclusion threshold.
    #[arg(long = "metering-priority-fee-percentile", default_value_t = DEFAULT_PRIORITY_FEE_PERCENTILE)]
    pub metering_priority_fee_percentile: f64,

    /// Priority fee (in wei) returned when a resource is not congested.
    #[arg(long = "metering-uncongested-priority-fee", default_value_t = DEFAULT_UNCONGESTED_PRIORITY_FEE)]
    pub metering_uncongested_priority_fee: u64,

    /// Number of recent blocks to retain in the metering cache.
    #[arg(
        long = "metering-cache-size",
        default_value_t = DEFAULT_METERING_CACHE_SIZE,
        value_parser = clap::value_parser!(usize).range(1..)
    )]
    pub metering_cache_size: usize,
}

impl Args {
    fn flashblocks_enabled(&self) -> bool {
        self.websocket_url.is_some()
    }

    fn metering_kafka_settings(&self) -> Option<MeteringKafkaSettings> {
        if !self.enable_metering {
            return None;
        }

        let brokers = self.metering_kafka_brokers.clone()?;
        let topic = self.metering_kafka_topic.clone()?;
        let group_id = self.metering_kafka_group_id.clone()?;

        Some(MeteringKafkaSettings {
            brokers,
            topic,
            group_id,
            properties_file: self.metering_kafka_properties_file.clone(),
        })
    }
}

#[derive(Clone)]
struct MeteringKafkaSettings {
    brokers: String,
    topic: String,
    group_id: String,
    properties_file: Option<String>,
}

#[derive(Clone)]
struct MeteringRuntime {
    cache: Arc<RwLock<MeteringCache>>,
    estimator: Arc<PriorityFeeEstimator>,
    tx_sender: mpsc::UnboundedSender<MeteredTransaction>,
    flashblock_sender: mpsc::UnboundedSender<FlashblockInclusion>,
}

struct CompositeFlashblocksReceiver<Client> {
    state: Arc<FlashblocksState<Client>>,
    /// Optional channel for the metering pipeline; flashblocks RPC still needs the stream even
    /// when metering is disabled, so we only forward inclusions if a sender is provided.
    metering_sender: Option<mpsc::UnboundedSender<FlashblockInclusion>>,
}

impl<Client> CompositeFlashblocksReceiver<Client> {
    fn new(
        state: Arc<FlashblocksState<Client>>,
        metering_sender: Option<mpsc::UnboundedSender<FlashblockInclusion>>,
    ) -> Self {
        Self {
            state,
            metering_sender,
        }
    }
}

impl<Client> base_reth_flashblocks_rpc::subscription::FlashblocksReceiver
    for CompositeFlashblocksReceiver<Client>
where
    FlashblocksState<Client>: base_reth_flashblocks_rpc::subscription::FlashblocksReceiver,
{
    fn on_flashblock_received(&self, flashblock: Flashblock) {
        metrics::counter!("metering.flashblocks.received").increment(1);
        metrics::gauge!("metering.flashblocks.index").set(flashblock.index as f64);
        metrics::gauge!("metering.flashblocks.transactions")
            .set(flashblock.diff.transactions.len() as f64);
        metrics::gauge!("metering.flashblocks.block_number")
            .set(flashblock.metadata.block_number as f64);

        self.state.on_flashblock_received(flashblock.clone());

        let Some(sender) = &self.metering_sender else {
            return;
        };
        let Some(inclusion) = flashblock_inclusion_from_flashblock(&flashblock) else {
            return;
        };

        if sender.send(inclusion).is_err() {
            warn!(
                target: "metering::flashblocks",
                "Failed to forward flashblock inclusion to metering"
            );
        }
    }
}

fn flashblock_inclusion_from_flashblock(flashblock: &Flashblock) -> Option<FlashblockInclusion> {
    if flashblock.diff.transactions.is_empty() {
        metrics::counter!("metering.flashblocks.empty").increment(1);
        return None;
    }

    let ordered_tx_hashes: Vec<TxHash> = flashblock
        .diff
        .transactions
        .iter()
        .map(|tx_bytes| TxHash::from(keccak256(tx_bytes)))
        .collect();

    Some(FlashblockInclusion {
        block_number: flashblock.metadata.block_number,
        flashblock_index: flashblock.index,
        ordered_tx_hashes,
    })
}

fn main() {
    let default_version_metadata = default_reth_version_metadata();
    try_init_version_metadata(RethCliVersionConsts {
        name_client: "Base Reth Node".to_string().into(),
        cargo_pkg_version: format!(
            "{}/{}",
            default_version_metadata.cargo_pkg_version,
            env!("CARGO_PKG_VERSION")
        )
        .into(),
        p2p_client_version: format!(
            "{}/{}",
            default_version_metadata.p2p_client_version, NODE_RETH_CLIENT_VERSION
        )
        .into(),
        extra_data: format!(
            "{}/{}",
            default_version_metadata.extra_data, NODE_RETH_CLIENT_VERSION
        )
        .into(),
        ..default_version_metadata
    })
    .expect("Unable to init version metadata");

    Cli::<OpChainSpecParser, Args>::parse()
        .run(|builder, args| async move {
            info!(message = "starting custom Base node");

            let flashblocks_enabled = args.flashblocks_enabled();
            let transaction_tracing_enabled = args.enable_transaction_tracing;
            let metering_enabled = args.enable_metering;
            let kafka_settings = args.metering_kafka_settings();
            if metering_enabled && kafka_settings.is_none() {
                bail!(
                    "Metering requires Kafka configuration: provide --metering-kafka-brokers, --metering-kafka-topic, and --metering-kafka-group-id"
                );
            }

            let op_node = OpNode::new(args.rollup_args.clone());

            let metering_runtime = if metering_enabled {
                let cache = Arc::new(RwLock::new(MeteringCache::new(args.metering_cache_size)));
                let limits = ResourceLimits {
                    gas_used: Some(args.metering_gas_limit),
                    execution_time_us: Some(args.metering_execution_time_us),
                    state_root_time_us: args.metering_state_root_time_us,
                    data_availability_bytes: Some(args.metering_da_bytes),
                };
                let estimator = Arc::new(PriorityFeeEstimator::new(
                    cache.clone(),
                    args.metering_priority_fee_percentile,
                    limits,
                    alloy_primitives::U256::from(args.metering_uncongested_priority_fee),
                ));

                let (tx_sender, tx_receiver) = mpsc::unbounded_channel::<MeteredTransaction>();
                let (flashblock_sender, flashblock_receiver) =
                    mpsc::unbounded_channel::<FlashblockInclusion>();

                let annotator_cache = cache.clone();
                tokio::spawn(async move {
                    ResourceAnnotator::new(annotator_cache, tx_receiver, flashblock_receiver)
                        .run()
                        .await;
                });

                Some(MeteringRuntime {
                    cache,
                    estimator,
                    tx_sender,
                    flashblock_sender,
                })
            } else {
                None
            };

            let metering_runtime_for_flashblocks = metering_runtime.clone();

            if let (Some(runtime), Some(settings)) = (metering_runtime.clone(), kafka_settings) {
                let mut client_config = ClientConfig::new();
                client_config.set("bootstrap.servers", &settings.brokers);
                client_config.set("group.id", &settings.group_id);
                client_config.set("enable.partition.eof", "false");
                client_config.set("session.timeout.ms", "6000");
                client_config.set("enable.auto.commit", "true");
                client_config.set("auto.offset.reset", "earliest");

                if let Some(path) = settings.properties_file.as_ref() {
                    match load_kafka_config_from_file(path) {
                        Ok(props) => {
                            for (key, value) in props {
                                client_config.set(key, value);
                            }
                        }
                        Err(err) => {
                            warn!(
                                message = "Failed to load Kafka properties file",
                                file = %path,
                                %err
                            );
                        }
                    }
                }

                let tx_sender = runtime.tx_sender.clone();
                let topic = settings.topic.clone();
                tokio::spawn(async move {
                    let config = KafkaBundleConsumerConfig {
                        client_config,
                        topic,
                    };

                    match KafkaBundleConsumer::new(config, tx_sender) {
                        Ok(consumer) => consumer.run().await,
                        Err(err) => error!(
                            target: "metering::kafka",
                            %err,
                            "Failed to initialize Kafka consumer"
                        ),
                    }
                });
            }

            let fb_cell: Arc<OnceCell<Arc<FlashblocksState<_>>>> = Arc::new(OnceCell::new());
            let metering_runtime_for_rpc = metering_runtime.clone();

            let NodeHandle {
                node: _node,
                node_exit_future,
            } = builder
                .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
                .with_components(op_node.components())
                .with_add_ons(op_node.add_ons())
                .on_component_initialized(move |_ctx| Ok(()))
                .install_exex_if(
                    transaction_tracing_enabled,
                    "transaction-tracing",
                    move |ctx| async move {
                        Ok(transaction_tracing_exex(
                            ctx,
                            args.enable_transaction_tracing_logs,
                        ))
                    },
                )
                .install_exex_if(flashblocks_enabled, "flashblocks-canon", {
                    let fb_cell = fb_cell.clone();
                    move |mut ctx| async move {
                        let fb = fb_cell
                            .get_or_init(|| Arc::new(FlashblocksState::new(ctx.provider().clone())))
                            .clone();
                        Ok(async move {
                            while let Some(note) = ctx.notifications.try_next().await? {
                                if let Some(committed) = note.committed_chain() {
                                    for b in committed.blocks_iter() {
                                        fb.on_canonical_block_received(b);
                                    }
                                    let _ = ctx.events.send(ExExEvent::FinishedHeight(
                                        committed.tip().num_hash(),
                                    ));
                                }
                            }
                            Ok(())
                        })
                    }
                })
                .extend_rpc_modules(move |ctx| {
                    if metering_enabled {
                        info!(message = "Starting Metering RPC");
                        let estimator = metering_runtime_for_rpc
                            .as_ref()
                            .map(|runtime| runtime.estimator.clone())
                            .expect("metering estimator should be initialized");
                        let metering_api = MeteringApiImpl::new(ctx.provider().clone(), estimator);
                        ctx.modules.merge_configured(metering_api.into_rpc())?;
                    }

                    if flashblocks_enabled {
                        info!(message = "Starting Flashblocks");

                        let ws_url = Url::parse(
                            args.websocket_url
                                .expect("WEBSOCKET_URL must be set when Flashblocks is enabled")
                                .as_str(),
                        )?;

                        let fb = fb_cell
                            .get_or_init(|| Arc::new(FlashblocksState::new(ctx.provider().clone())))
                            .clone();
                        fb.start();

                        let metering_sender = metering_runtime_for_flashblocks
                            .as_ref()
                            .map(|runtime| runtime.flashblock_sender.clone());
                        let receiver = Arc::new(CompositeFlashblocksReceiver::new(
                            fb.clone(),
                            metering_sender,
                        ));

                        let mut flashblocks_client = FlashblocksSubscriber::new(receiver, ws_url);
                        flashblocks_client.start();

                        let api_ext = EthApiExt::new(
                            ctx.registry.eth_api().clone(),
                            ctx.registry.eth_handlers().filter.clone(),
                            fb,
                        );

                        ctx.modules.replace_configured(api_ext.into_rpc())?;
                    } else {
                        info!(message = "flashblocks integration is disabled");
                    }
                    Ok(())
                })
                .launch_with_fn(|builder| {
                    let engine_tree_config = TreeConfig::default()
                        .with_persistence_threshold(builder.config().engine.persistence_threshold)
                        .with_memory_block_buffer_target(
                            builder.config().engine.memory_block_buffer_target,
                        );

                    let launcher = EngineNodeLauncher::new(
                        builder.task_executor().clone(),
                        builder.config().datadir(),
                        engine_tree_config,
                    );

                    builder.launch_with(launcher)
                })
                .await?;

            node_exit_future.await
        })
        .unwrap();
}
