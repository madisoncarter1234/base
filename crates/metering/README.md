# Transaction Metering RPC

RPC endpoints for simulating and metering transaction bundles on Optimism.

## `base_meterBundle`

Simulates a bundle of transactions, providing gas usage and execution time metrics. The response format is derived from `eth_callBundle`, but the request uses the [TIPS Bundle format](https://github.com/base/tips) to support TIPS's additional bundle features.

**Parameters:**

The method accepts a Bundle object with the following fields:

- `txs`: Array of signed, RLP-encoded transactions (hex strings with 0x prefix)
- `block_number`: Target block number for bundle validity (note: simulation always uses the latest available block state)
- `min_timestamp` (optional): Minimum timestamp for bundle validity (also used as simulation timestamp if provided)
- `max_timestamp` (optional): Maximum timestamp for bundle validity
- `reverting_tx_hashes` (optional): Array of transaction hashes allowed to revert
- `replacement_uuid` (optional): UUID for bundle replacement
- `flashblock_number_min` (optional): Minimum flashblock number constraint
- `flashblock_number_max` (optional): Maximum flashblock number constraint
- `dropping_tx_hashes` (optional): Transaction hashes to exclude from bundle

**Returns:**
- `bundleGasPrice`: Average gas price
- `bundleHash`: Bundle identifier
- `coinbaseDiff`: Total gas fees paid
- `ethSentToCoinbase`: ETH sent directly to coinbase
- `gasFees`: Total gas fees
- `stateBlockNumber`: Block number used for state (always the latest available block)
- `totalGasUsed`: Total gas consumed
- `totalExecutionTimeUs`: Total execution time (μs)
- `results`: Array of per-transaction results:
  - `txHash`, `fromAddress`, `toAddress`, `value`
  - `gasUsed`, `gasPrice`, `gasFees`, `coinbaseDiff`
  - `ethSentToCoinbase`: Always "0" currently
  - `executionTimeUs`: Transaction execution time (μs)

**Example:**

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "base_meterBundle",
  "params": [{
    "txs": ["0x02f8...", "0x02f9..."],
    "blockNumber": 1748028,
    "minTimestamp": 1234567890,
    "revertingTxHashes": []
  }]
}
```

Note: While some fields like `revertingTxHashes` are part of the TIPS Bundle format, they are currently ignored during simulation. The metering focuses on gas usage and execution time measurement.

## Implementation

- Executes transactions sequentially using Optimism EVM configuration
- Tracks microsecond-precision execution time per transaction
- Stops on first failure
- Automatically registered in `base` namespace

## Upcoming Features

- **In-memory metering cache:** Maintains per-flashblock resource snapshots (gas, DA bytes,
  execution time, state-root time) for the latest 12 blocks to support pricing decisions.
- **Stream ingestion:** Background tasks will hydrate the cache by consuming the TIPS Kafka
  feed (for timing metrics) and the flashblocks websocket stream (for inclusion order).
- **Priority-fee estimator:** Aggregates cached data in ascending priority-fee order to
  project the fee a bundle must pay to satisfy each resource constraint, including
  percentile-based recommendations.
- **`base_meteredPriorityFeePerGas` RPC:** Accepts a TIPS Bundle, meters it locally, and
  responds with per-resource fee suggestions for each flashblock index plus aggregated
  min/max guidance for next-block and next-flashblock inclusion.

## Testing & Observability Plan

- **Unit coverage:** Exercise cache eviction, transaction ordering, and estimator threshold
  logic with synthetic datasets (see `cache.rs` and `estimator.rs` tests). Extend with Kafka
  parsing tests once the ingest message schema is integrated.
- **Integration harness:** Feed mocked Kafka + websocket streams into the ingest pipeline to
  validate cache hydration and end-to-end RPC responses. Leverage existing async test
  utilities in the workspace for deterministic sequencing.
- **Property-style checks:** Generate random transaction fee/usage distributions to ensure the
  estimator produces monotonic thresholds and sensible percentiles across resource types.
- **Metrics & tracing:** Emit gauges for cache freshness (latest block/index), Kafka lag,
  websocket heartbeat, and estimator latency. Reuse the existing `tracing` instrumentation
  pattern in the repo so operators can alert on stale data paths.
