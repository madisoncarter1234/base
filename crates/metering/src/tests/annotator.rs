#![allow(clippy::unwrap_used)]

use super::utils::init_tracing;
use crate::{FlashblockInclusion, MeteredTransaction, MeteringCache, ResourceAnnotator};
use alloy_primitives::{B256, U256};
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::Duration;

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
async fn annotator_updates_cache_on_flashblock() {
    init_tracing();

    let cache = Arc::new(RwLock::new(MeteringCache::new(12)));
    let (tx_sender, tx_receiver) = mpsc::unbounded_channel::<MeteredTransaction>();
    let (flash_sender, flash_receiver) = mpsc::unbounded_channel::<FlashblockInclusion>();

    let annotator = ResourceAnnotator::new(cache.clone(), tx_receiver, flash_receiver);

    let handle = tokio::spawn(annotator.run());

    // Send a metered transaction (goes to pending map).
    let tx_hash = B256::random();
    let transaction = test_transaction(tx_hash, 5);
    tx_sender.send(transaction.clone()).unwrap();

    // Give time for the event to be processed.
    tokio::time::sleep(Duration::from_millis(25)).await;

    // Cache should still be empty - tx is in pending, not cache.
    assert!(cache.read().is_empty());

    // Now send the flashblock inclusion event with the actual location.
    flash_sender
        .send(FlashblockInclusion {
            block_number: 1,
            flashblock_index: 0,
            ordered_tx_hashes: vec![tx_hash],
        })
        .unwrap();

    // Give time for the flashblock event to be processed.
    tokio::time::sleep(Duration::from_millis(25)).await;

    // Now the cache should have the transaction at the correct location.
    let cache_read = cache.read();
    let block = cache_read.block(1).expect("block inserted");
    assert_eq!(block.flashblock_count(), 1);
    let flashblock = block.flashblock(0).expect("flashblock present");
    assert_eq!(flashblock.len(), 1);
    assert_eq!(
        flashblock.transactions().next().unwrap().tx_hash,
        transaction.tx_hash
    );

    drop(cache_read);
    drop(tx_sender);
    drop(flash_sender);

    handle.await.unwrap();
}

#[tokio::test]
async fn annotator_ignores_unknown_tx_hashes() {
    init_tracing();

    let cache = Arc::new(RwLock::new(MeteringCache::new(12)));
    let (tx_sender, tx_receiver) = mpsc::unbounded_channel::<MeteredTransaction>();
    let (flash_sender, flash_receiver) = mpsc::unbounded_channel::<FlashblockInclusion>();

    let annotator = ResourceAnnotator::new(cache.clone(), tx_receiver, flash_receiver);
    let handle = tokio::spawn(annotator.run());

    // Send a metered transaction.
    let tx_hash = B256::random();
    tx_sender.send(test_transaction(tx_hash, 8)).unwrap();

    tokio::time::sleep(Duration::from_millis(20)).await;

    // Send flashblock inclusion with a *different* tx hash (not in pending).
    let unknown_hash = B256::random();
    flash_sender
        .send(FlashblockInclusion {
            block_number: 10,
            flashblock_index: 2,
            ordered_tx_hashes: vec![unknown_hash],
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(20)).await;

    // Cache should be empty since the tx hash didn't match.
    assert!(cache.read().is_empty());

    // The original tx is still in pending (not matched yet).
    // Send a flashblock that matches it.
    flash_sender
        .send(FlashblockInclusion {
            block_number: 10,
            flashblock_index: 3,
            ordered_tx_hashes: vec![tx_hash],
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(20)).await;

    // Now it should be in the cache.
    let cache_read = cache.read();
    let block = cache_read.block(10).expect("block inserted");
    assert_eq!(block.flashblock(3).unwrap().len(), 1);

    drop(cache_read);
    drop(tx_sender);
    drop(flash_sender);
    handle.await.unwrap();
}
