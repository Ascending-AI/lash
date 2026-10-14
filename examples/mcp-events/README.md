# MCP events

MCP servers can publish events ([SEP-3415](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/3415)):
a ticket was created, a build failed, an email arrived. The MCP plugin turns
each event into durable lash work, and it is off unless the host turns it on,
one server at a time.

`ticket_events()` is the host's policy for one server: deliver by push or
poll, and let a model react in its own session. `watch_tickets` subscribes a
session as the host:

```rust,ignore
let plugin = Arc::new(lash::mcp::McpPluginFactory::new(servers).await?);
let core = lash::LashCore::standard_builder(backend).plugin(plugin.clone()) /* … */ .build(owner)?;
core.bind_mcp_events(&plugin.events());
let subscription = mcp_events::watch_tickets(&plugin.events(), "tickets", &session).await?;
```

The subscription is a durable process of the plugin's engine. It polls (or
holds a stream), saves the server's cursor in the same commit that admits the
event's delivery, and delivers each event as one keyed turn of the session.
A crash anywhere, a node handover or the server delivering an event twice
starts no second turn. A model can subscribe too, through `subscribe_event`,
within what the host's reaction profile allows.

`status` reads what a host needs to know without running anything: events
delivered, gaps where the server resumed past its retention, failed
deliveries, and why a subscription is parked. `resume` un-parks one;
`unsubscribe` ends one, after which no event reacts.

The test in `src/tests.rs` runs the whole path against the plugin's SEP-3415
fixture server over SQLite. The laws live in
`crates/lash-durable-test/tests/mcp_events_laws.rs` and
`crates/lash-durable-test/tests/mcp_events_crash_proof.rs`.
