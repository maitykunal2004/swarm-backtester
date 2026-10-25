# swarm-backtester — BTCUSDT market-prediction research system (data layer)

Rust research harness for BTCUSDT spot data from Binance. This first pass is the
**skeleton + data layer only**: there are no trained models, no strategy and no
execution simulator yet. Everything that exists is keyless, offline-testable and
built around one rule: **no decision may see the future**.

The repository also contains an older Vite/React dashboard under `src/`,
`package.json` and `vite.config.js`; it is untouched by this pass.

## Status at a glance

| Layer | State |
|---|---|
| `mb-data` — Binance klines ingestion, pagination, rate limiting, immutable raw cache | **Implemented** |
| `mb-features` — point-in-time features, fixed-horizon targets/labels, train-only normalization, leakage self-test | **Implemented** |
| `mb-tracker` — append-only experiment tracking | **Implemented** |
| `mb-models` — `Forecaster` trait, `ProbabilityForecast` validation, `StubForecaster` | **Stubbed** (returns `NotImplemented`) |
| `mb-backtest` — cost/config types, `Simulator` entry point | **Stubbed** (returns `NotImplemented`) |
| `mb-cli` — `mb fetch`, `build-dataset`, `cache-info`, `selftest`, `status` | **Implemented** |

`cargo test --workspace` is green (unit + integration tests, offline).

## Quickstart

Requires a stable Rust toolchain (`cargo`, edition 2021).

```bash
# 1. Sanity-check the leakage guards (offline, synthetic data).
cargo run -p mb-cli -- selftest

# 2. Fetch one day of BTCUSDT 1m klines into data/cache.
cargo run -p mb-cli -- fetch \
  --symbol BTCUSDT --interval 1m \
  --start 2024-01-01 --end 2024-01-02 \
  --cache-dir data/cache

# 3. Build a point-in-time dataset with a 12-bar horizon and a train/holdout split.
cargo run -p mb-cli -- build-dataset \
  --cache-dir data/cache --symbol BTCUSDT --interval 1m \
  --decision-every-bars 1 --horizon-bars 12 --flat-band-bps 10 \
  --warmup-bars 64 --train-end 2024-01-01T12:00:00Z \
  --out data/datasets/btc-1m-h12 --tracker-dir data/runs

# 4. Inspect what is cached.
cargo run -p mb-cli -- cache-info --cache-dir data/cache
cargo run -p mb-cli -- status
```

`--start`/`--end` accept RFC3339 (`2024-01-01T00:00:00Z`), `YYYY-MM-DD`, or epoch
milliseconds. Fetch defaults to `https://data-api.binance.vision`, Binance's
official keyless market-data endpoint (same `/api/v3/klines` schema). Use
`--base-url https://api.binance.com` where the main API is reachable; some
regions receive HTTP 451 from the main API.

## Workspace

```
Cargo.toml                 # workspace, shared deps, lean dev profile
crates/data/               # keyless REST, pagination, rate limiting, raw cache
tests/                     # mock-transport pagination/retry/immutability tests
crates/features/           # point-in-time features, labels, normalization, guards
tests/                     # dataset pipeline + leakage self-test
crates/models/             # STUB: interfaces only
crates/backtest/           # STUB: cost/config contracts only
crates/tracker/            # append-only experiment runs
tests/                     # immutability and metrics tests
crates/cli/                # `mb` binary
```

## 1. Data ingestion (`mb-data`)

- **Keyless public REST only.** No API key, no signing, no account endpoints.
- **Pagination.** Walks `[start, end)` in `limit <= 1000` pages, advancing by the
  last returned open time. Stops on an empty/short page, a complete range, or an
  explicit page cap. A page that does not advance is an error, never a silent loop.
- **Rate-limit aware.** Local rolling-minute weight budget (conservative default
  1200/min, `--weight-budget-per-minute`), correct klines weight table, sleeps
  instead of hammering the endpoint, folds in `X-MBX-USED-WEIGHT-1M`, honors
  `Retry-After` on 429/418 (cap 300 s), exponential backoff + jitter on 5xx and
  transport errors, bounded retries.
- **Raw cache is append-only.** Each successful response body is written
  **verbatim** to a content-addressed, timestamped file; no transformation and no
  overwrite. Layout:

  ```
  data/cache/binance-spot/klines/BTCUSDT/1m/
    <start>_<limit>_<end>_<fetched_at>_<sha8>.json       # raw exchange bytes
    ... .meta.json                                       # provenance sidecar
    index.jsonl                                          # append-only catalog
  ```

  Sidecars record request parameters, fetch time, status, weight header,
  SHA-256, byte/record counts and first/last kline times. A repeated fetch reuses
  the cache; `--refresh` fetches again and **adds** a new file beside the old one.
  Reads select the newest snapshot per exact request key; every superseded file
  stays on disk (`cache-info` reports both `pages` and `raw_files`).
- **Bounded memory.** One page body (<= 1000 klines) is in memory at a time;
  callbacks stream each page to the dataset builder. Response bodies are capped
  at 8 MiB.

## 2. Point-in-time datasets and targets (`mb-features`)

### Decision timestamps and horizon

A decision is made at the **close time of a completed bar**. Decision cadence is
fixed on the interval grid: with `--decision-every-bars 1` every bar is a decision;
with `3`, every third bar on the grid. The horizon is a fixed number of bars
(`--horizon-bars`).

For decision bar `t`:

- **Features** use bars `0..=t` only, via `PointInTime`, which exposes
  `&bars[..=t]` and cannot reach `t+1`.
- **Label entry** is the **open of bar `t+1`** (strictly after the decision time).
- **Label exit** is the **close of bar `t+H`**. The label window must be contiguous;
  gapped or incomplete windows are dropped and counted, never imputed.

### Features (v1, 10 columns)

`ret_1_bps`, `ret_5_bps`, `ret_15_bps`, `realized_vol_20_bps`, `range_bps`,
`close_location`, `sma20_gap_bps`, `volume_z_20`, `taker_buy_ratio_20`,
`trades_z_20`. All are deterministic, finite-checked, and computed only from the
point-in-time view.

### Forecast target

For every decision the target is explicit:

- categorical probabilities `p_down`, `p_flat`, `p_up` over the forward log
  return, with `|return| <= flat_band_bps` classified flat;
- `expected_magnitude_abs` / `magnitude_bps`: realized absolute forward log return;
- plus `fwd_log_return`, `fwd_return_bps`, `direction` and entry/exit timestamps
  for evaluation.

Labels are currently **one-hot** realizations of the categorical target (a valid
cross-entropy target); the row schema is designed so soft/quantile target makers
can replace the `p_*` values later without changing consumers.

### Outputs

`build-dataset` writes to an output directory:

- `dataset.raw.csv` — raw feature values + labels + split;
- `dataset.normalized.csv` — features z-scored with **training-window-only**
  mean/std (omitted when no `--train-end`);
- `manifest.json` — spec, feature names, row counts, drops, cache provenance,
  SHA-256 of both CSVs, and the fitted normalization statistics.

Dataset outputs refuse to overwrite by default (`--output-policy fail`); use
`overwrite` explicitly. Raw cache remains untouched either way.

## 3. No-leakage guards

1. **Type-level visibility.** Feature code receives only `PointInTime`, which
   exposes bars up to the decision bar. Dataset rows are built from that view.
2. **Runtime label guard.** `compute_label` rejects an entry timestamp that is not
   strictly after the decision timestamp, non-contiguous label windows, and
   non-positive prices.
3. **Train-only normalization.** Welford statistics accept only `Split::Train`
   rows with `decision_time_ms < train_end_ms`; every other row raises
   `FeatureError::Leakage`. Fitting happens during the streaming pass; the second
   pass applies the frozen statistics to every row.
4. **Incomplete horizons are dropped**, and the count is reported in the manifest.
5. **Leakage self-test** (`cargo run -p mb-cli -- selftest`, also a test) proves:
   - corrupting **all data after** a decision leaves that row's features
     bit-identical;
   - corrupting **all data at/before** a decision changes features but leaves its
     labels bit-identical;
   - labels do depend on future data (they change when future data is corrupted);
   - features do depend on history (they change when history is corrupted);
   - normalization statistics are invariant to future corruption and reject
     future rows, including a future row relabelled as train;
   - label math matches a hand-computed forward return and every label starts
     strictly after its decision.

A run prints 16 named checks and exits non-zero if any fails.

## 4. Experiment tracking (`mb-tracker`)

Runs live under `<tracker-dir>/runs/<run_id>/`:

- immutable `run.json` (git commit, config hash, dataset fingerprint, notes),
- immutable `config.json`,
- append-only `metrics.jsonl` and `events.jsonl`,
- one `summary.json` at finalization (a second finalize is an error).

`build-dataset --tracker-dir ...` records the dataset build, its spec and the raw
CSV fingerprint.

## 5. Stubbed on purpose

- **Models:** `mb_models::StubForecaster` validates feature length and returns
  `ModelError::NotImplemented`. `ProbabilityForecast` and its validation already
  match the target schema.
- **Backtest:** `mb_backtest::Simulator::run` returns `NotImplemented`;
  `SimulationConfig`/`CostModel` define fees, slippage, capital and risk fraction
  so the next pass cannot quietly ignore frictions.

No training, walk-forward evaluation, position sizing or PnL numbers are claimed
in this pass.

## 6. Memory profile

The data path is streaming by construction: one cache page, a feature buffer of
`warmup + horizon + 2` bars, and CSV rows written as they are produced. The
normalization pass re-reads the CSV rather than holding the dataset in RAM.

Measured peak RSS on the included smoke run (real Binance 1m klines):

| Command | Workload | Peak RSS |
|---|---|---|
| `mb selftest` | 600 synthetic bars, 277 rows | ~8.4 MB |
| `mb fetch` | 1440 real bars, 2 pages | ~8.5 MB |
| `mb build-dataset` | 1440 bars → 1364 rows, 2 CSVs | ~8.5 MB |

This is far below the 300 MB budget. The bound does not grow with history length,
only with the page size and the fixed feature window.

## 7. Tests

```bash
cargo test --workspace                      # offline, green
cargo test -p mb-data -- --ignored          # adds a live keyless Binance smoke test
```

Coverage includes: kline parsing/validation, pagination, cache reuse, refresh
append-only behavior, identical-page dedupe, Retry-After/5xx retry policy,
end-of-data handling, overlapping-page series reads, dataset row/label/CSV
semantics, train-only normalization, output overwrite policy, tracker
append-only behavior, and the full leakage self-test.

## 8. Next pass (not started)

Baseline models trained on `dataset.normalized.csv`, walk-forward splits,
calibration of `p_*`, magnitude regression, cost-aware simulator, and swarm
parallelism — all on top of this data contract.
