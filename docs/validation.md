# Validation and reproducibility

## Full historical comparison

The port was compared with the original reference result using the full captured
universe, before reducing any data for the test fixture:

- Window: 2026-10-29 13:30:39 UTC through 2026-10-30 13:30:39 UTC.
- Markets: 26,169.
- Trade rows: 1,051,158.
- Entries: 12 at $5 each.
- Settled winners/losers: 11/1.
- Invested: $60.00.
- Net simulated PnL: $23.48526089333893.
- ROI on costs: 39.14210148889821%.
- Unpriced positions: 0.
- Collection errors: 0.

All individual trade fields, aggregate values and skip counts matched exactly.
The implementation uses stable print ordering, round-trip JSON float parsing,
and compensated sums to preserve numerical threshold behavior.

The raw capture's SHA-256 and full-universe comparison are recorded in
[validation/full-24h-parity.json](../validation/full-24h-parity.json). The large
original capture is not included. Its contents can be replayed with the native
CLI if available.

## Included regression fixture

`tests/fixtures/historical/` contains the twelve executed markets and their
complete captured trade histories, stripped of unrelated metadata. Its sibling
`results.json` is the expected reference result. The native test loads both and
requires exact agreement on every trade field and summary value.

```sh
cargo test --locked historical_fixture_matches_all_twelve_reference_trades_exactly
cargo run --release --locked -- replay --input tests/fixtures/historical/raw.json
```

This selected-market fixture is for regression testing only. It does not contain
the other 26,157 markets, cannot validate discovery completeness, and must not be
used as an unbiased universe for strategy tuning or performance claims.

No interpreter or network access is needed to run the historical fixture test.

## Selection overlap

The rules were selected from 2,592 configurations on the twelve-hour fitting
window. The 24-hour replay includes those same hours. Outside the fitting window,
seven entries earned $10.601346986623328 on $35, but those results are still
retrospective rather than a prospective live trading trial.

Trade-print liquidity proxies and assumed costs differ from executable fills.
Live eligibility checks can reduce the number of alerts. There is no capital
cap or correlation adjustment, and the sample is short.

## Automated checks

The native tests exercise:

- Strict price-move thresholds, stale quotes and future-data exclusion.
- Correct binary-token selection and reversal configuration.
- Duplicate/same-second fills and stable ordering.
- Fee-inclusive budgets, decimal tick rounding and minimum shares.
- Cold-start baselines and one alert per condition.
- First-print rejection without later-price selection.
- Metadata/book retries, deadline changes and closed markets.
- Cursor failures, incomplete histories and stale/future feeds.
- Marking, settlement cutoff, missing valuations and null ROI.
- Durable pending recovery, ledger recovery and process locks.
- Frozen strategy fingerprint compatibility and historical parity.

CI runs formatting, strict Clippy and native tests. Live API smoke tests are
manual, bounded, and never submit orders.
