# MCP events as host code

This reference host uses only the `lash` facade for MCP transport and keyed
turn admission. Its own SQLite database holds subscription registrations,
opaque cursors, retention-gap evidence, immutable mapped inputs and delivery
receipts. Lash owns admitted turn execution and recovery.

Create the plugin with each server enabled by
`McpServerConfig::with_events(McpEventTransportPolicy::default())`, and obtain
`plugin.events()`. The handle provides typed `list`, `poll` and `open`.
A push request offers `read_window` and explicit `close`; hosts choose the
window duration, item threshold and per-server reorder-settle interval.
Listings share `McpDataLimits.catalog_pages`, `catalog_items`, `catalog_bytes`
(defaults: 64 pages, 4,096 items, 8 MiB). The default notification capacity is
1,024 and reorder settle is 50 ms. Overflow returns typed continuity loss:
reconnect and reconcile from committed progress. The settle interval accounts
for rmcp's independently dispatched notification handlers; it cannot prove
that an arbitrary late notification was received.

`HostStore::register` records this host's explicit destination and arguments
under an id and generation. Change those by registering a new generation and
disabling the old one. `run_polling` implements the host's poll/dispatch loop
with an explicit `PollPolicy` and shutdown future; it returns on faults or gaps
for host reconciliation. `poll_once` is also available for a different host
scheduler. Neither enables model-facing tools nor registers a Lash engine.

`HostStore::accept` commits the whole occurrence batch and its cursor in one
transaction. The host maps each ticket to an existing session and immutable
input, excluding transport cursors. A delivery key hashes the server, event
type, subscription id, generation and occurrence id; its stable `TurnId`
keys `durable.send(input).id(turn)`. `dispatch_pending` submits each committed
input, then acknowledges it in a separate host transaction. A crash between
those writes leaves a pending row and retries the same input with the same key.
The SQLite law loses that acknowledgment, reopens the host database, retries
and observes exactly one turn.

This is a single-dispatcher reference. A distributed host adds its own lease
and fencing, authorization, retry/backoff, gap recovery and retention policy.
A disabled subscription stops polling; already accepted inbox deliveries remain
owed. Keep host dedup evidence for as long as the source can redeliver. A host
using keyed process starts must record the returned binding before pruning can
remove Lash's start-key evidence. Hosts can use their own `ProcessEngine`
through the generic seam when their dispatcher needs durable process steps.
