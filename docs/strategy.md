# Strategy and execution semantics

## Signal

The frozen model checks each UTC minute during the final hour before Gamma's
scheduled `endDate`. It prefers the YES token as the first outcome; otherwise it
uses the market's first binary outcome.

At decision time `t`, compare the last first-outcome print at or before `t`
with the last print at or before `t - 5 minutes`. Both observations must be no
more than 120 seconds old relative to their target timestamps. The absolute
move must be strictly greater than five percentage points.

Count taker-only activity in `(t - 5 minutes, t]` across both tokens. Require
$100 notional, five fills, and two nonempty wallet addresses. The preceding
nonoverlapping window `(t - 10 minutes, t - 5 minutes]` must also contain at
least $100. Recent notional must be at least the preceding notional.

A rise selects the first outcome, a fall the actual opposite token. The config
schema also supports reversal, but the frozen model uses momentum. Neumaier
compensated sums preserve the original numerical activity thresholds.

## Entry

The first same-token print at least five seconds after the signal is the entry
proxy. It must appear within two minutes and before scheduled close. A $5 entry
cannot exceed 10% of that print's dollar notional. Apply 1% adverse slippage and
reject prices at or above $1 or above the 75¢ strategy cap. The $5 budget includes
the assumed 0.5% fee:

```text
proxy price = print price × 1.01
shares      = $5 / (proxy price × 1.005)
```

Only the first qualifying signal per condition is consumed. An invalid first
entry print does not permit selecting a more favorable later print.

Live alerts also require:

1. Fresh market metadata confirms order acceptance and an open market.
2. The scheduled deadline and selected token mapping match the pending signal.
3. Current asks fill the entire budget including assumed fees.
4. The limit is rounded down to the market tick, bounded by the proxy and 75¢.
5. Estimated shares meet the market minimum.
6. The entry print, global feed, and returned book timestamp remain fresh.

The default freshness ceiling is 30 seconds. Requests that fail for metadata or
books retry pending decisions while fresh. Price, liquidity, minimum-size, deadline
or time rejection consumes the condition's first signal.

## Valuation in replay

Settlement is recognized only with explicit resolved metadata, a recorded close
no later than cutoff, and exact binary payouts of 0 and 1. Otherwise, use a
same-token print at cutoff no older than 120 seconds, with the configured adverse
slippage and exit fee. Missing marks stay unpriced.

Reported ROI divides valued PnL by the costs of valued entries, not by starting
portfolio capital. There is no portfolio capital or correlation constraint.

## Limits

A trade print does not prove historical executable depth. The live book gate is
an additional constraint and can reject signals that historical replay accepted.
Gamma deadlines can represent event starts rather than settlement times, and
metadata can change. Current metadata cannot recover historical deadline edits
or delisted markets.

The parameters were selected on twelve hours of history. The later 24-hour
comparison overlaps that fitting window. One profitable day and correlated
crypto-market entries do not establish a durable edge.

Official APIs:
[trade feed](https://docs.polymarket.com/api-reference/feeds/list-trades),
[market discovery](https://docs.polymarket.com/api-reference/markets/list-markets).
