use crate::{MeteredTransaction, TxMeteringEvent};
use alloy_consensus::Transaction;
use alloy_consensus::transaction::Recovered;
use alloy_eips::Encodable2718;
use alloy_primitives::U256;
use eyre::Result;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_flz::tx_estimated_size_fjord_bytes;
use rdkafka::consumer::{CommitMode, Consumer, StreamConsumer};
use rdkafka::{ClientConfig, Message};
use std::time::Duration;
use tips_core::types::AcceptedBundle;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::sleep;
use tracing::{debug, error, trace, warn};

/// Configuration required to connect to the Kafka topic publishing accepted bundles.
pub struct KafkaBundleConsumerConfig {
    pub client_config: ClientConfig,
    pub topic: String,
}

/// Consumes `AcceptedBundle` events from Kafka and publishes transaction-level metering data.
pub struct KafkaBundleConsumer {
    consumer: StreamConsumer,
    tx_sender: UnboundedSender<TxMeteringEvent>,
    topic: String,
}

impl KafkaBundleConsumer {
    pub fn new(
        config: KafkaBundleConsumerConfig,
        tx_sender: UnboundedSender<TxMeteringEvent>,
    ) -> Result<Self> {
        let KafkaBundleConsumerConfig {
            client_config,
            topic,
        } = config;

        let consumer: StreamConsumer = client_config.create()?;
        consumer.subscribe(&[topic.as_str()])?;

        Ok(Self {
            consumer,
            tx_sender,
            topic,
        })
    }

    /// Starts listening for Kafka messages until the task is cancelled.
    pub async fn run(self) {
        loop {
            match self.consumer.recv().await {
                Ok(message) => {
                    if let Err(err) = self.handle_message(message).await {
                        error!(target: "metering::kafka", error = %err, "Failed to process Kafka message");
                    }
                }
                Err(err) => {
                    error!(
                        target: "metering::kafka",
                        error = %err,
                        "Kafka receive error for topic {}. Retrying in 1s",
                        self.topic
                    );
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn handle_message(&self, message: rdkafka::message::BorrowedMessage<'_>) -> Result<()> {
        let payload = message
            .payload()
            .ok_or_else(|| eyre::eyre!("Kafka message missing payload"))?;

        let bundle: AcceptedBundle = serde_json::from_slice(payload)?;
        debug!(
            target: "metering::kafka",
            block_number = bundle.block_number,
            uuid = %bundle.uuid(),
            "Received accepted bundle from Kafka"
        );

        if let Some(flashblock_index) = bundle.meter_bundle_response.state_flashblock_index {
            self.publish_transactions(&bundle, flashblock_index)?;
        } else {
            warn!(
                target: "metering::kafka",
                block_number = bundle.block_number,
                uuid = %bundle.uuid(),
                "Skipping bundle without flashblock index"
            );
        }

        // Best-effort asynchronous commit.
        if let Err(err) = self.consumer.commit_message(&message, CommitMode::Async) {
            warn!(
                target: "metering::kafka",
                error = %err,
                "Failed to commit Kafka offset asynchronously"
            );
        }

        Ok(())
    }

    fn publish_transactions(&self, bundle: &AcceptedBundle, flashblock_index: u64) -> Result<()> {
        if bundle.txs.len() != bundle.meter_bundle_response.results.len() {
            warn!(
                target: "metering::kafka",
                bundle_uuid = %bundle.uuid(),
                tx_count = bundle.txs.len(),
                result_count = bundle.meter_bundle_response.results.len(),
                "Bundle transactions/results length mismatch; skipping"
            );
            return Ok(());
        }

        for (tx, result) in bundle
            .txs
            .iter()
            .zip(bundle.meter_bundle_response.results.iter())
        {
            let priority_fee_per_gas = calculate_priority_fee(tx);
            let data_availability_bytes = tx_estimated_size_fjord_bytes(&tx.encoded_2718());

            let metered_tx = MeteredTransaction {
                tx_hash: tx.tx_hash(),
                priority_fee_per_gas,
                gas_used: result.gas_used,
                execution_time_us: result.execution_time_us,
                state_root_time_us: 0,
                data_availability_bytes,
            };

            let event = TxMeteringEvent {
                block_number: bundle.block_number,
                flashblock_index,
                transaction: metered_tx,
            };

            if let Err(err) = self.tx_sender.send(event) {
                warn!(
                    target: "metering::kafka",
                    error = %err,
                    "Failed to send metered transaction event"
                );
            }
        }

        trace!(
            target: "metering::kafka",
            block_number = bundle.block_number,
            flashblock_index,
            transactions = bundle.txs.len(),
            "Published metering events for bundle"
        );

        Ok(())
    }
}

fn calculate_priority_fee(tx: &Recovered<OpTxEnvelope>) -> U256 {
    tx.max_priority_fee_per_gas()
        .map(U256::from)
        .unwrap_or_else(|| U256::from(tx.max_fee_per_gas()))
}
