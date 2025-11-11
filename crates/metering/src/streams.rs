use crate::{MeteredTransaction, MeteringCache};
use alloy_primitives::TxHash;
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, error, warn};

/// Message emitted by the Kafka ingest task once a transaction has been processed.
#[derive(Debug)]
pub struct TxMeteringEvent {
    pub block_number: u64,
    pub flashblock_index: u64,
    pub transaction: MeteredTransaction,
}

/// Message received from the flashblocks websocket feed.
#[derive(Debug)]
pub struct FlashblockInclusion {
    pub block_number: u64,
    pub flashblock_index: u64,
    /// Tx hashes included in this flashblock in priority-fee order.
    pub ordered_tx_hashes: Vec<TxHash>,
}

/// Output sent to downstream components when a flashblock snapshot is ready.
#[derive(Debug)]
pub struct FlashblockSnapshot {
    pub block_number: u64,
    pub flashblock_index: u64,
    pub transactions: Vec<MeteredTransaction>,
}

/// Handles ingestion of Kafka metrics and websocket inclusion events, writing into the cache.
pub struct StreamsIngest {
    cache: Arc<RwLock<MeteringCache>>,
    tx_updates_rx: UnboundedReceiver<TxMeteringEvent>,
    flashblock_rx: UnboundedReceiver<FlashblockInclusion>,
    snapshot_tx: UnboundedSender<FlashblockSnapshot>,
}

impl StreamsIngest {
    pub fn new(
        cache: Arc<RwLock<MeteringCache>>,
        tx_updates_rx: UnboundedReceiver<TxMeteringEvent>,
        flashblock_rx: UnboundedReceiver<FlashblockInclusion>,
        snapshot_tx: UnboundedSender<FlashblockSnapshot>,
    ) -> Self {
        Self {
            cache,
            tx_updates_rx,
            flashblock_rx,
            snapshot_tx,
        }
    }

    pub async fn run(mut self) {
        loop {
            tokio::select! {
                Some(tx_event) = self.tx_updates_rx.recv() => {
                    self.handle_tx_event(tx_event);
                }
                Some(flashblock_event) = self.flashblock_rx.recv() => {
                    self.handle_flashblock_event(flashblock_event);
                }
                else => {
                    break;
                }
            }
        }
    }

    fn handle_tx_event(&mut self, event: TxMeteringEvent) {
        debug!(
            block_number = event.block_number,
            flashblock_index = event.flashblock_index,
            tx_hash = %event.transaction.tx_hash,
            "Inserting metered transaction into cache"
        );
        let mut cache = self.cache.write();
        cache.upsert_transaction(
            event.block_number,
            event.flashblock_index,
            event.transaction,
        );
    }

    fn handle_flashblock_event(&mut self, event: FlashblockInclusion) {
        let transactions = {
            let cache = self.cache.read();
            if let Some(flashblock) = cache.flashblock(event.block_number, event.flashblock_index) {
                let mut tx_by_hash: HashMap<_, _> = flashblock
                    .transactions()
                    .map(|tx| (tx.tx_hash, tx.clone()))
                    .collect();
                event
                    .ordered_tx_hashes
                    .iter()
                    .filter_map(|hash| tx_by_hash.remove(hash))
                    .collect::<Vec<_>>()
            } else {
                warn!(
                    block_number = event.block_number,
                    flashblock_index = event.flashblock_index,
                    "Flashblock inclusion arrived before transactions were cached"
                );
                return;
            }
        };

        if transactions.is_empty() {
            warn!(
                block_number = event.block_number,
                flashblock_index = event.flashblock_index,
                "Received flashblock inclusion with no matching cached transactions"
            );
            return;
        }

        let snapshot = FlashblockSnapshot {
            block_number: event.block_number,
            flashblock_index: event.flashblock_index,
            transactions,
        };

        if let Err(e) = self.snapshot_tx.send(snapshot) {
            error!(error = %e, "Failed to send flashblock snapshot");
        }
    }
}
