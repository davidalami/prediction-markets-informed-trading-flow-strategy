# Runtime data

The monitor creates its alert ledger, restart state and lock files here.
Replay outputs and redirected heartbeat logs can also go here.

Generated contents are ignored by Git. Keep the ledger and state together when
restarting or moving a monitor, and use separate paths for different rules.
