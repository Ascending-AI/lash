# Delegation

Lash ships no subagent implementation. Creating a session is explicit and only
a fork clones ([ADR 0134](../../docs/adr/0134-creating-a-session-is-explicit-only-a-fork-clones.md)):
the core knows sessions, an optional parent link and forks. This library shows
that the `lash` facade is enough for a host to build delegation itself.

`DelegationPluginFactory` contributes one tool, `spawn_agent` (`agents.spawn`
in a cell). A call:

1. builds the child's `SessionCreateRequest` from what the host configured
   (the child's `SessionSpec`, tool access, an optional prompt plan and RLM
   termination) and from the call's task, seed and output shape. It reads
   nothing of the parent session; the parent link the request records is
   lineage for display and audit only;
2. declares one `SessionTurn` process start that creates the child and runs
   its first turn, under the lifetime the host's policy chooses;
3. parks the call on that start, so the child's final value resolves the
   parent's call.

What a delegated child is lives in the plugin's own config namespace
(`DelegatedChild`), stated in the child's create request. A delegated child is
offered `submit_error` (`task.fail`) and a prompt section instead of
`spawn_agent`, so it cannot delegate again.

```rust,ignore
let delegation = delegation::DelegationPluginFactory::new(
    lash::SessionSpec::new("child-model", lash::TurnBudget::bounded(8), lash::MaxToolCalls::new(64)),
    lash::process::lifetime::starter,
)
.with_rlm_children(lash::rlm::RlmFinalAnswerFormat::RawFinalValue);
```

Register the factory as a host plugin. Cancel delegated work through its
process or scope: the parent link has no effect on the child session.

The laws live in `src/tests.rs` (where a spawn parents its child, what the
child's request states, which tools each role is offered) and in
`crates/lash-durable-test/tests/core_node_processes.rs`, which runs a child
through this tool on the core's served node.
