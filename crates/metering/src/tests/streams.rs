#![allow(clippy::unwrap_used)]

use super::utils::init_tracing;
use crate::{
    FlashblockInclusion, FlashblockSnapshot, MeteredTransaction, MeteringCache, StreamsIngest,
    TxMeteringEvent,
};
use alloy_primitives::{B256, U256};
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

fn test_transaction(tx_hash: B256, priority: u64) -> MeteredTransaction {
    MeteredTransaction {
        tx_hash,
        priority_fee_per_gas: U256::from(priority),
        gas_used: 21_000,
        execution_time_us: 500,
        state_root_time_us: 0,
        data_availability_bytes: 120,
    }
}

#[tokio::test]
async fn streams_ingest_updates_cache_and_emits_snapshot() {
    init_tracing();

    let cache = Arc::new(RwLock::new(MeteringCache::new(12)));
    let (tx_sender, tx_receiver) = mpsc::unbounded_channel::<TxMeteringEvent>();
    let (flash_sender, flash_receiver) = mpsc::unbounded_channel::<FlashblockInclusion>();
    let (snapshot_sender, mut snapshot_receiver) = mpsc::unbounded_channel::<FlashblockSnapshot>();

    let ingest = StreamsIngest::new(cache.clone(), tx_receiver, flash_receiver, snapshot_sender);

    let ingest_handle = tokio::spawn(ingest.run());

    let tx_hash = B256::random();
    let transaction = test_transaction(tx_hash, 5);
    tx_sender
        .send(TxMeteringEvent {
            block_number: 1,
            flashblock_index: 0,
            transaction: transaction.clone(),
        })
        .unwrap();

    // Ensure the transaction is present in the cache.
    tokio::time::sleep(Duration::from_millis(25)).await;
    let cache_read = cache.read();
    let block = cache_read.block(1).expect("block inserted");
    assert_eq!(block.flashblock_count(), 1);
    let flashblock = block.flashblock(0).expect("flashblock present");
    assert_eq!(flashblock.len(), 1);

    drop(cache_read);

    flash_sender
        .send(FlashblockInclusion {
            block_number: 1,
            flashblock_index: 0,
            ordered_tx_hashes: vec![tx_hash],
        })
        .unwrap();

    let snapshot = timeout(Duration::from_secs(1), snapshot_receiver.recv())
        .await
        .expect("snapshot timeout")
        .expect("snapshot not none");

    assert_eq!(snapshot.block_number, 1);
    assert_eq!(snapshot.flashblock_index, 0);
    assert_eq!(snapshot.transactions.len(), 1);
    assert_eq!(snapshot.transactions[0].tx_hash, transaction.tx_hash);

    drop(tx_sender);
    drop(flash_sender);
    drop(snapshot_receiver);

    // Wait for ingest task to exit cleanly.
    ingest_handle.await.unwrap();
}

#[tokio::test]
async fn streams_ingest_filters_missing_transactions() {
    init_tracing();

    let cache = Arc::new(RwLock::new(MeteringCache::new(12)));
    let (tx_sender, tx_receiver) = mpsc::unbounded_channel::<TxMeteringEvent>();
    let (flash_sender, flash_receiver) = mpsc::unbounded_channel::<FlashblockInclusion>();
    let (snapshot_sender, mut snapshot_receiver) = mpsc::unbounded_channel::<FlashblockSnapshot>();

    let ingest = StreamsIngest::new(cache.clone(), tx_receiver, flash_receiver, snapshot_sender);
    let ingest_handle = tokio::spawn(ingest.run());

    // Populate cache with one transaction.
    let tx_hash = B256::random();
    tx_sender
        .send(TxMeteringEvent {
            block_number: 10,
            flashblock_index: 2,
            transaction: test_transaction(tx_hash, 8),
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(20)).await;

    // Send inclusion referencing a missing transaction hash.
    flash_sender
        .send(FlashblockInclusion {
            block_number: 10,
            flashblock_index: 2,
            ordered_tx_hashes: vec![B256::random()],
        })
        .unwrap();

    // Ensure no snapshot is produced for mismatched hashes.
    match timeout(Duration::from_millis(200), snapshot_receiver.recv()).await {
        Ok(Some(snapshot)) => panic!(
            "unexpected snapshot produced: block {} flashblock {}",
            snapshot.block_number, snapshot.flashblock_index
        ),
        Ok(None) | Err(_) => {
            // Expected path: either the channel closed with no data, or timed out without messages.
        }
    }

    drop(tx_sender);
    drop(flash_sender);
    drop(snapshot_receiver);
    ingest_handle.await.unwrap();
}
