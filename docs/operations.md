# Operating the monitor

## Startup and latency

The default strategy needs thirteen minutes of rolling history including a
warmup margin. Startup pages backward through the global taker-only feed until
that floor is reached; a pagination cap or error suppresses decisions.

The first successful poll establishes a baseline and begins new decisions at
the next UTC minute. Subsequent polls overlap the last feed timestamp by 30
seconds and deduplicate fills, including fills sharing a timestamp. Same-second
print order is preserved. Native HTTP connections are pooled.

The global feed ignores start/end bounds. A changing `end` query refreshes the
response-cache key; timestamps, rather than query bounds, determine coverage.
Metadata and book queries also refresh cache keys. The first live test found a
five-minute cached global response, which is why cache freshness is explicit.
Upstream indexing and network delays still apply.

Each loop targets a five-second period, measured from the start of the poll.
Slow polls extend the cadence. Do not interpret polling as a subsecond streaming
guarantee. Old decision times are counted as missed instead of replayed as fresh
trade opportunities.

## State and alert ledger

The state and alert file paths are independently locked with advisory OS locks.
The locks interoperate with the previous monitor on Unix. Keep only one monitor
using either resource.

For an eligible alert, the commit order is:

1. Append the alert JSON to the ledger and sync it.
2. Atomically replace and sync the state file.
3. Emit the heartbeat, terminal alert, and optional macOS notification.

Startup replays the ledger into state. A crash after step 1 cannot produce a
second alert for that condition, even if state replacement did not complete.
A crash before a desktop notification can lose that notification; the durable
alert is still present in the ledger.

Pending signals persist across restart, but the freshness and deadline gates
still apply. Completed conditions are retained in memory/state for seven days;
the ledger remains the durable alert history. Corrupt state or ledger files
stop startup. Use a new state/ledger pair when changing strategy rules.

## CLI controls

```sh
informed-trader-flow monitor --help
```

| Option | Default | Meaning |
|---|---|---|
| `--strategy` | `config/strategy.json` | Rule file and selection provenance |
| `--state` | `data/flow-monitor-state.json` | Restart state |
| `--alerts` | `data/flow-alerts.jsonl` | Durable eligible alerts |
| `--interval` | 5 | Target polling period, 1–60 seconds |
| `--max-pages` | 200 | Trade-feed pagination cap per poll |
| `--max-age` | 30 | Freshness bound, 5–60 seconds |
| `--count` | unlimited | Exit after this many polls |
| `--notify` | off | Add macOS desktop notifications |

Default file paths are anchored to the repository location used to build the
binary. Explicit paths support relocated binaries and independent instances.

## Output

Stdout is NDJSON: one startup record and one heartbeat per poll. Heartbeats
contain status, coverage/pages, feed lag, buffered rows, pending entries, eligible
signals, skips, and errors.

Stderr carries readable alerts and notification failures. Market titles and
notification text are passed as arguments, never interpolated into shell or
AppleScript source. The notification helper is macOS's built-in `osascript`;
Rust handles all market data, strategy, state, and replay logic.

Ctrl-C requests shutdown; a currently blocked HTTP call can take up to its
15-second timeout to return. No order submission, key loading or account
authentication occurs.
