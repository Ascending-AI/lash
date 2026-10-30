//! What a code cell restored from its snapshot keeps (FIG-5224, FIG-5225;
//! ADR 0132 §5–§8), on the production turn driver over the durable store.
//!
//! A lash core's session holds one sent input; its turn runs on the
//! production session activation with the RLM protocol, a scripted model
//! and host tools whose bodies write to an [`ExternalWorld`] that survives
//! every node. The nodes are simulated (A and B). The cells:
//!
//! - **Identity:** the cell calls `ext_write`, declared `Once`, twice,
//!   operation A then B. The matrix cuts the uncut run at every labelled
//!   write under every fault and recovers on the other node: wherever it
//!   was cut, A and B never share a `ToolCallId`, so a recipient that
//!   deduplicates on it (ADR 0132 §7) never suppresses B as A's repeat. A
//!   cut after A settled and before B's quiet point restores the cell onto
//!   A's outcome, and B is the next admitted operation, not A again.
//! - **Sleep:** the cell sleeps for `N`. Its idle session releases as
//!   `waiting` once its idle eviction passes, and its timer row persists.
//!   Node A is killed at `N / 2` and B started: nobody claims the session
//!   before the deadline, and B wakes the cell once at the deadline it was
//!   admitted with, not at recovery + `N`.
//! - **Repeatable:** the cell calls `ext_retry`, declared `Repeatable`.
//!   The call is its own admitted execution under its declared policy, so
//!   a cut at its admission, or at its lost outcome commit, re-runs it at
//!   the same ordinal: every body entry sees the one call id, and the cell
//!   is answered with the tool's value, never an interruption.
//! - **Race:** the cell races `ext_fast` against `ext_slow`, whose body
//!   runs until it is cancelled. Both calls are admitted executions: every
//!   body entry is an admitted member's, and the loser settles on its own
//!   after the race answered.
//! - **Ends once (FIG-5999):** a cell whose end no commit of its own
//!   records ends with the commit that consumes its answer. A node killed
//!   between the two leaves the cell at its last park, and the node that
//!   takes it over runs its final stretch again, which issued no effect:
//!   every body has run once, the model was asked twice in total, the
//!   turn's report holds the cell's calls once, and no execution of the
//!   cell is left open. A cell that ends with an execution still open
//!   closes it and records its end first.
//! - **Paid response (FIG-6000, FIG-6230):** the model response that asked
//!   for a cell is committed on receipt, before the cell runs. A cancel the
//!   turn accepted before the cell's first save admits nothing, and a node
//!   lost in a first stretch that outlasts its slice costs no provider
//!   request.
//! - **Await process:** the cell awaits a host process with
//!   `processes.await`. The call parks on its completion wait, the session
//!   releases as `waiting` while the process runs, and the cell resumes
//!   with the process's outcome once it ends.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/call_cancel.rs"]
mod call_cancel;
use call_cancel::{CancelCase, CancelSource, CommitOrder, written_x};

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};

use matrix::MatrixTestExt as _;

#[path = "support/packages.rs"]
mod packages;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::LlmOutputPart;
use lash_core::ToolCall;
use lash_core::ToolCallId;
use lash_core::ToolOutcome;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core_execution::ToolCallOutput;
use lash_core_execution::ToolCancellation;
use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::runtime::actor::round::{AdmittedExecution, MemberBody};
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_durable::domain::WaitKind;
use lash_durable::runner::Activation;
use lash_durable::{
    ActorDispatch, ActorKey, ActorState, CommitLabel, DurableInstant, DurableStore,
};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, Script, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire,
};
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::backend::BackendParts;
use lash_execution_contracts::backend::StoreSet;
use lash_execution_contracts::process::EngineAction;
use lash_execution_contracts::process::EngineEvent;
use lash_execution_contracts::process::EngineState;
use lash_execution_contracts::process::EngineStateFormat;
use lash_execution_contracts::process::LifetimeDecision;
use lash_execution_contracts::process::ProcessEngine;
use lash_execution_contracts::process::ProcessInfraError;
use lash_execution_contracts::process::ProcessInput;
use lash_execution_contracts::process::ProcessOutcome;
use lash_execution_contracts::process::ProcessProvenance;
use lash_execution_contracts::process::ProcessRecord;
use lash_execution_contracts::process::ProcessRegistration;
use lash_execution_contracts::process::StepRequest;
use lash_kernel_value::identity::SessionId;
use lash_kernel_value::tool_contract::ExecutionPolicy;
use lash_protocol_types::sync::MutexExt as _;
use lash_vocabulary::claims::ObligationToken;
use lash_vocabulary::identity::ProcessId;
use lash_vocabulary::identity::RunIdCoreSupport as _;
use serde_json::{Value, json};

use dialect::Dialect;

const SESSION: &str = "cell-restore-session";
const WRITE: &str = "ext_write";
const RETRY: &str = "ext_retry";
const FAST: &str = "ext_fast";
const SLOW: &str = "ext_slow";
const HANDLE: &str = "ext_handle";
/// The tool no catalog lists: the cell reaches it through the deferred
/// resolver's grant.
const GRANTED: &str = "ext_granted";
/// The tool a redeployed catalog binds at the granted tool's path.
const AMBIENT: &str = "ext_ambient";
/// The path the deferred resolver refuses until the deployment is
/// redeployed, with [`REFUSED_REASON`].
const REFUSED: &str = "ext_refused";
const REFUSED_REASON: &str = "ext_refused is refused by the host's policy";
/// The path the deferred resolver has no tool for until the deployment is
/// redeployed, with [`MISSING_NOTE`].
const MISSING: &str = "ext_missing";
const MISSING_NOTE: &str = "ext_missing is not installed on this host";
const MODEL: &str = "cell-restore-model";
/// What the final answer starts with.
const FINAL: &str = "final answer";
/// How long the sleeping cell sleeps, in virtual milliseconds: well past a
/// lease's failover, so a recovery at its half is long before its end.
const SLEEP_MS: u64 = 120_000;
/// How long the awaited process sleeps, in virtual milliseconds: past the
/// session's idle eviction, so the parked cell releases its session
/// before the process ends, and within `processes.await`'s limit (the 2 min
/// tool default), which its park never outlives.
const PROCESS_MS: u64 = 100_000;
/// The awaited process's engine.
const ENGINE: &str = "cell-await-law";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// Which cell the model writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cell {
    /// Two `Once` operations: A writes `x: 1`, then B writes `x: 2`.
    Identity,
    /// One durable sleep of [`SLEEP_MS`].
    Sleep,
    /// One `Repeatable` call.
    Repeatable,
    /// A race of a fast call against a slow one.
    Race,
    ExplicitCancel(CancelCase),
    /// A `processes.await` of a host process.
    AwaitProcess,
    /// A granted call, a durable sleep of [`SLEEP_MS`], and the granted
    /// call again.
    Deferred,
    /// A granted call whose resolution, before the cell's first save,
    /// accepts the turn's `Immediate` cancel.
    CancelledBeforeSave,
    /// A first stretch of many slices, then one `Once` call.
    LongStretch,
}

impl Cell {
    fn source(self) -> String {
        let body = match self {
            Self::Identity => "const a = await tools.extWrite({ x: 1 });\n\
                 const b = await tools.extWrite({ x: 2 });\n\
                 console.log([a, b]);"
                .to_owned(),
            Self::Sleep => format!("await sleep({SLEEP_MS});\nconsole.log(\"woke\");"),
            Self::Repeatable => {
                "const a = await tools.extRetry({ x: 1 });\nconsole.log(a);".to_owned()
            }
            Self::Race => "const w = await Promise.race([tools.extFast({ x: 1 }), \
                 tools.extSlow({ x: 2 })]);\nconsole.log(w);"
                .to_owned(),
            Self::ExplicitCancel(case) if case.source == CancelSource::Host => {
                let join = if case.explicit {
                    "try { await a; } catch (e) { if (e.data && e.data.origin === 'operator_requested' && e.data.message === 'cancel the selected call' && e.data.raw.request_id === 'host-call-cancel' && e.data.raw.origin === 'host-race-loser') { await tools.extWrite({ x: 4 }); } }"
                } else {
                    ""
                };
                format!(
                    "const a = tools.extSlow({{ x: 1 }});\nconst b = tools.extSlow({{ x: 2 }});\nawait tools.extFast({{}});\n{join}\nawait tools.extWrite({{ x: 3 }});\nawait sleep(60000);\nconsole.log('cell stayed alive');"
                )
            }
            Self::ExplicitCancel(case)
                if matches!(
                    case.source,
                    CancelSource::TypeScript | CancelSource::TypeScriptShared
                ) =>
            {
                call_cancel::typescript_source(case)
            }
            Self::ExplicitCancel(case) => r#"import asyncio
async def selected():
    try:
        await tools.ext_slow({"x": 1})
    finally:
        await tools.ext_write({"x": 3})
a = asyncio.create_task(selected())
b = asyncio.create_task(tools.ext_slow({"x": 2}))
await tools.ext_fast({})
a.cancel()
try:
    await a
except asyncio.CancelledError:
    await tools.ext_write({"x": 4})
await asyncio.sleep(60)
print("cell stayed alive")"#
                .replace(
                    "a.cancel()",
                    if case.explicit {
                        "a.cancel()"
                    } else {
                        "await tools.ext_write({\"x\": 3})"
                    },
                )
                .replace(
                    "    await a\nexcept",
                    if case.explicit {
                        "    await a\nexcept"
                    } else {
                        "    pass\nexcept"
                    },
                ),
            Self::AwaitProcess => "const h = await tools.extHandle({});\n\
                 const r = await processes.await({ handle: h });\n\
                 console.log(r);"
                .to_owned(),
            Self::Deferred => format!(
                "let r = ''; try {{ await dynamic.extRefused({{}}); }} catch (e) {{ r = String(e); }}\n\
                 let u = ''; try {{ await dynamic.extMissing({{}}); }} catch (e) {{ u = String(e); }}\n\
                 const a = await dynamic.extGranted({{ x: 1 }});\n\
                 await sleep({SLEEP_MS});\n\
                 let r2 = ''; try {{ await dynamic.extRefused({{}}); }} catch (e) {{ r2 = String(e); }}\n\
                 let u2 = ''; try {{ await dynamic.extMissing({{}}); }} catch (e) {{ u2 = String(e); }}\n\
                 const b = await dynamic.extGranted({{ x: 2 }});\n\
                 console.log([a, b, r, u, r2, u2]);"
            ),
            Self::CancelledBeforeSave => {
                "const a = await dynamic.extGranted({ x: 1 });\nconsole.log(a);".to_owned()
            }
            Self::LongStretch => format!(
                "let n = 0;\n\
                 for (let i = 0; i < {STRETCH}; i++) {{ n = n + i; }}\n\
                 const a = await tools.extWrite({{ x: 1 }});\n\
                 console.log([n, a]);"
            ),
        };
        body.to_string()
    }
}

/// How many loop turns [`Cell::LongStretch`]'s first stretch runs: many
/// times [`STRETCH_SLICE`]'s charge, well within the cell's instruction
/// bound.
const STRETCH: u32 = 20_000;
/// The charge units one slice of a [`Cell::LongStretch`] run may spend.
const STRETCH_SLICE: u64 = 20_000;

/// The outside world: every body entry of every host call, by call. It
/// survives every node.
#[derive(Default)]
struct ExternalWorld {
    entries: Mutex<BTreeMap<ToolCallId, Vec<(String, Value)>>>,
    /// The process `ext_handle` hands out.
    process: Mutex<Option<ProcessId>>,
    /// How many times the deferred resolver was asked. One ask carries
    /// every dotted call path of a cell that no catalog tool answers, the
    /// dialect's own `console.log` among them.
    resolved: std::sync::atomic::AtomicUsize,
    /// Whether the deployment was redeployed: its resolver grants nothing
    /// and its catalog binds [`AMBIENT`] at the granted tool's path.
    redeployed: std::sync::atomic::AtomicBool,
    /// The format of the execution binding the resolver's grant carries:
    /// none when it carries none.
    binding: Mutex<Option<lash_kernel_value::providers::FormatVersion>>,
    /// How many `ext_slow` bodies were told of their call's cancel.
    cancelled: std::sync::atomic::AtomicUsize,
    signalled: Mutex<Vec<i64>>,
    cleanup_signals: Mutex<Option<Vec<i64>>>,
    cancel_case: Mutex<Option<CancelCase>>,
    authority_violations: Mutex<Vec<String>>,
    cancel_backend: Mutex<Option<Backend>>,
    cancel_host: Mutex<Option<(lash::DurableSession, lash::RunId)>>,
    cleanup_children: Mutex<Option<Vec<(i64, bool)>>>,
    ready: Mutex<std::collections::BTreeSet<i64>>,
    changed: tokio::sync::Notify,
    final_presenting: std::sync::atomic::AtomicBool,
    fast_returned: std::sync::atomic::AtomicBool,
    nodes: Mutex<std::sync::Weak<SimNodes>>,
}

impl ExternalWorld {
    fn entries(&self) -> BTreeMap<ToolCallId, Vec<(String, Value)>> {
        self.entries.lock_recover().clone()
    }
}

struct ExtTools {
    world: Arc<ExternalWorld>,
}

#[async_trait::async_trait]
impl StaticToolExecute for ExtTools {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.world
            .entries
            .lock_recover()
            .entry(call.context.call_id().clone())
            .or_default()
            .push((call.tool_id().as_str().to_owned(), call.args.clone()));
        match call.tool_id().as_str() {
            SLOW => {
                let case = *self.world.cancel_case.lock_recover();
                if case.is_some_and(|case| case.order == CommitOrder::TypedCancel)
                    && written_x(call.args) == Some(1)
                {
                    return call_cancel::authority::typed_cancel(&self.world, call).await;
                }
                let declared = if case.is_some() {
                    let payload = json!({ "callcancel_x": written_x(call.args).unwrap() });
                    let launch =
                        lash_core_execution::testing::held_engine_launch(call.context, &payload)
                            .await
                            .unwrap();
                    let start_cx = call.context.start_cx().unwrap();
                    let start = lash_execution_contracts::tool_intent::StartProcessIntent {
                        owner: lash_vocabulary::identity::RuntimeOwner::Session(
                            call.context.session_id().unwrap().clone(),
                        ),
                        declaration:
                            lash_execution_contracts::process::ProcessStartDeclaration::engine(
                                lash_execution_contracts::testing::HELD_PROCESS_ENGINE_KIND,
                                payload,
                                launch,
                                lash_execution_contracts::process::ProcessOriginator::Session {
                                    session_id: call.context.session_id().unwrap().clone(),
                                    agent_frame_id: Some(
                                        call.context.agent_frame_id().unwrap().clone(),
                                    ),
                                },
                                lash_execution_contracts::process::lifetime::starter(&start_cx),
                            ),
                    };
                    Some(start)
                } else {
                    None
                };
                if written_x(call.args) == Some(1)
                    && case.is_some_and(|case| case.order == CommitOrder::Final)
                {
                    return lash_core::ToolAttemptOutcome::done(
                        lash_core::ToolOutcomeDone::ok(json!({ "final": 1 }))
                            .with_state(call_cancel::increment_state()),
                        lash_execution_contracts::tool_intent::ToolIntents::v3(vec![
                            lash_execution_contracts::tool_intent::ToolIntent::StartProcess(
                                Box::new(declared.unwrap()),
                            ),
                        ]),
                    );
                }
                if written_x(call.args) == Some(1)
                    && case.is_some_and(|case| case.order == CommitOrder::PlainFinal)
                {
                    return lash_core::ToolAttemptOutcome::done_without_intents(
                        lash_core::ToolOutcomeDone::ok(json!({ "final": 1 }))
                            .with_state(call_cancel::increment_state()),
                    );
                }
                if let Some(hint) = case.and_then(|case| case.hint) {
                    let mut pending = lash_core::PendingCompletion::new();
                    pending.on_cancel = hint;
                    return lash_core::ToolAttemptOutcome::pending(
                        pending.resolved_by_declared_start(
                            call.context.declare_start(declared.unwrap()).unwrap(),
                        ),
                    );
                }
                if case.is_some_and(|case| case.order.final_wins()) {
                    loop {
                        let changed = self.world.changed.notified();
                        tokio::pin!(changed);
                        changed.as_mut().enable();
                        if self
                            .world
                            .fast_returned
                            .load(std::sync::atomic::Ordering::SeqCst)
                        {
                            break;
                        }
                        changed.await;
                    }
                    return lash_core::ToolAttemptOutcome::done_without_intents(
                        lash_core::ToolOutcomeDone::ok(json!({ "sibling": true }))
                            .with_state(call_cancel::increment_state()),
                    );
                }
                self.world
                    .ready
                    .lock_recover()
                    .insert(written_x(call.args).unwrap());
                self.world.changed.notify_waiters();
                // Runs until its call is cancelled.
                match call.context.cancellation_token() {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending().await,
                }
                self.world
                    .signalled
                    .lock_recover()
                    .push(written_x(call.args).unwrap());
                self.world
                    .cancelled
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if let Some(start) = declared {
                    // A late success asks to launch after the cancel. The fence must refuse it.
                    lash_core::ToolAttemptOutcome::done(
                        lash_core::ToolOutcomeDone::ok(json!({ "late": true })),
                        lash_execution_contracts::tool_intent::ToolIntents::v3(vec![
                            lash_execution_contracts::tool_intent::ToolIntent::StartProcess(
                                Box::new(start),
                            ),
                        ]),
                    )
                } else {
                    ToolOutcome::err_fmt("ext_slow was cancelled").into()
                }
            }
            FAST if self.world.cancel_case.lock_recover().is_some() => {
                let case = self.world.cancel_case.lock_recover().unwrap();
                let backend = self.world.cancel_backend.lock_recover().clone().unwrap();
                if case.order == CommitOrder::TypedCancel {
                    call_cancel::wait_typed_cancel(&self.world).await;
                } else if case.order == CommitOrder::PlainFinal {
                    call_cancel::wait_plain_final(&self.world).await;
                } else if case.hint.is_some() {
                    call_cancel::wait_pending_calls(&self.world).await;
                    assert_eq!(
                        cancel_children(&backend).await.len(),
                        2,
                        "both pending calls must have registered their children"
                    );
                } else {
                    loop {
                        let changed = self.world.changed.notified();
                        tokio::pin!(changed);
                        changed.as_mut().enable();
                        let ready = if case.order == CommitOrder::Final {
                            self.world
                                .final_presenting
                                .load(std::sync::atomic::Ordering::SeqCst)
                        } else {
                            self.world.ready.lock_recover().len() == case.slow_calls()
                        };
                        if ready {
                            break;
                        }
                        changed.await;
                    }
                }
                if case.source == CancelSource::Host && case.explicit {
                    call_cancel::host_request(&self.world).await;
                }
                self.world
                    .fast_returned
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                self.world.changed.notify_waiters();
                ToolOutcome::ok(json!({ "barrier": true })).into()
            }
            WRITE if written_x(call.args) == Some(3) => {
                let backend = self.world.cancel_backend.lock_recover().clone();
                if let Some(backend) = backend {
                    let children = cancel_children(&backend)
                        .await
                        .into_iter()
                        .map(|child| {
                            let ProcessInput::Engine { payload, .. } = child.input.as_ref() else {
                                unreachable!()
                            };
                            (
                                payload["callcancel_x"].as_i64().unwrap(),
                                child.cancel_request.is_some(),
                            )
                        })
                        .collect();
                    self.world
                        .cleanup_children
                        .lock_recover()
                        .get_or_insert(children);
                }
                self.world
                    .cleanup_signals
                    .lock_recover()
                    .get_or_insert_with(|| self.world.signalled.lock_recover().clone());
                ToolOutcome::ok(json!({ "cleanup": true })).into()
            }
            HANDLE => {
                let process = self
                    .world
                    .process
                    .lock_recover()
                    .clone()
                    .expect("the host registered the awaited process");
                ToolOutcome::ok(lash_protocol_types::handle::handle_record_json(
                    &lash_protocol_types::handle::HandleId::process(&process),
                ))
                .into()
            }
            name => ToolOutcome::ok(json!({ "ran": name, "with": call.args })).into(),
        }
    }
}

fn definition(
    name: &str,
    policy: ExecutionPolicy,
    output: Value,
) -> lash_vocabulary::tool_contract::ToolDefinition {
    definition_at(name, &format!("tools.{name}"), policy, output)
}

/// The tool `name` at `operation`.
fn definition_at(
    name: &str,
    operation: &str,
    policy: ExecutionPolicy,
    output: Value,
) -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        name,
        operation,
        "A host call the cell restore laws watch.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "x": { "type": "number" } }
        }),
        output,
    )
    .expect("the tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(policy)
}

/// The definition the resolver grants for `tools.ext_granted`, or the one
/// a redeployed catalog binds there.
fn granted(name: &str) -> lash_vocabulary::tool_contract::ToolDefinition {
    definition_at(
        name,
        &format!("dynamic.{GRANTED}"),
        ExecutionPolicy::Once,
        json!({ "type": "object" }),
    )
}

/// The host tools of the deferred cell: the listed ones, the grant's body,
/// and, once redeployed, an ambient tool at the granted tool's path.
struct DeferredTools {
    world: Arc<ExternalWorld>,
    listed: Arc<dyn lash_core::ToolProvider>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for DeferredTools {
    fn tool_manifests(
        &self,
        config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok({
            let mut manifests = self.listed.tool_manifests(config)?;
            if self
                .world
                .redeployed
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                manifests.push(granted(AMBIENT).manifest());
            }
            manifests
        })
    }

    fn resolve_manifest_by_id(
        &self,
        id: &lash_vocabulary::tool_contract::ToolId,
        config: &lash_core::OwnerConfigView,
    ) -> Option<lash_vocabulary::tool_contract::ToolManifest> {
        [GRANTED, AMBIENT]
            .into_iter()
            .map(granted)
            .find(|definition| definition.id() == id)
            .map(|definition| definition.manifest())
            .or_else(|| self.listed.resolve_manifest_by_id(id, config))
    }

    fn resolve_contract(
        &self,
        name: &str,
        config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        if name == format!("dynamic.{GRANTED}") {
            return Some(Arc::new(granted(GRANTED).contract()));
        }
        self.listed.resolve_contract(name, config)
    }

    /// It reads its bindings in formats 1 and 2: a grant in format 1 is an
    /// older format it still reads.
    fn readable_binding_formats(&self) -> &'static [lash_kernel_value::providers::FormatVersion] {
        const READS: [lash_kernel_value::providers::FormatVersion; 2] = [
            lash_kernel_value::providers::FormatVersion::ONE,
            match lash_kernel_value::providers::FormatVersion::new(2) {
                Some(format) => format,
                None => lash_kernel_value::providers::FormatVersion::ONE,
            },
        ];
        &READS
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        ExtTools {
            world: Arc::clone(&self.world),
        }
        .execute(call)
        .await
    }
}

/// Grants `tools.ext_granted` until the deployment is redeployed, and
/// counts every ask. With `cancel`, its first ask, which runs inside the
/// cell before the cell's first save, has the host request the turn's
/// `Immediate` cancel and waits for it to be accepted.
struct GrantingResolver {
    world: Arc<ExternalWorld>,
    cancel: Option<Arc<CancelTarget>>,
}

/// The turn a resolver cancels: the backend the host requests through, and
/// the run its send opened.
#[derive(Default)]
struct CancelTarget {
    turn: Mutex<Option<(Backend, lash_vocabulary::identity::TurnId)>>,
}

impl CancelTarget {
    /// Request the turn's `Immediate` cancel, as a host would.
    async fn request(&self) {
        let Some((backend, run)) = self.turn.lock_recover().clone() else {
            return;
        };
        let answer = lash_core::runtime::durable::session::request_turn_cancel(
            &backend,
            lash_durable::domain::TurnCancelRequest {
                session: session(),
                run,
                request_id: "cancel-before-the-first-save".to_owned(),
                origin: None,
                reason: None,
                undelivered: lash_core::TurnCancelUndeliveredInputPolicy::Defer,
                mode: lash_core::TurnCancelMode::Immediate,
            },
        )
        .await
        .expect("the cancel request commits");
        assert_eq!(answer, lash_durable::domain::TurnCancelAnswer::Requested);
    }
}

#[async_trait::async_trait]
impl lash_core::plugin::packages::DeferredPackageProvider for GrantingResolver {
    fn validate_grant(
        &self,
        grant: &lash_kernel_value::package::DeferredGrant,
        registry: lash_vocabulary::claims::RegistryKind,
    ) -> Result<(), lash_execution_contracts::process::linking::AdmissionRefusal> {
        packages::validate_grant("tools", "deferred", "dynamic", grant, registry)
    }
    async fn resolve(
        &self,
        _: &lash_core::DeferredResolveContext<'_>,
        _: &lash_kernel_value::package::DeferredGrant,
        requested: &lash_kernel_value::names::QualifiedName,
    ) -> Result<
        lash_core::plugin::packages::DeferredAnswer,
        lash_core::plugin::packages::ResolverFault,
    > {
        use lash_core::plugin::packages::DeferredAnswer;
        let first = self
            .world
            .resolved
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0;
        if first && let Some(cancel) = &self.cancel {
            cancel.request().await;
        }
        if self
            .world
            .redeployed
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(DeferredAnswer::Unavailable { note: None });
        }
        Ok(match requested.member.as_str() {
            GRANTED => {
                let codec = lash::plugins::ToolProviderHandle::new(
                    lash_vocabulary::claims::RegistryKind::Session,
                    "tools",
                    lash::tool_provider_key!("tools"),
                );
                let binding = (*self.world.binding.lock_recover()).map(|format| {
                    lash::tools::ExecutionBinding {
                        codec: codec.clone(),
                        format,
                        payload: json!({ "route": GRANTED }),
                    }
                });
                let definition = granted(GRANTED);
                let runner = lash::plugins::ToolProviderHandle::new(
                    lash_vocabulary::claims::RegistryKind::Session,
                    "tools",
                    lash::tool_provider_key!("tools"),
                );
                let route = lash_core_store::tool_surface::OperationRoute::resolved(
                    runner,
                    definition.clone(),
                    binding,
                );
                DeferredAnswer::Resolved {
                    definition: Box::new(definition),
                    route: Box::new(route),
                }
            }
            REFUSED => DeferredAnswer::Refused {
                note: REFUSED_REASON.to_owned(),
            },
            MISSING => DeferredAnswer::Unavailable {
                note: Some(MISSING_NOTE.to_owned()),
            },
            _ => DeferredAnswer::Unavailable { note: None },
        })
    }
}

async fn cancel_children(backend: &Backend) -> Vec<ProcessRecord> {
    lash_core::testing::process_roster_records_for_fixture(backend.process_registry().as_ref(),
        &lash_execution_contracts::process::ProcessListFilter {
            status: lash_execution_contracts::process::ProcessStatusFilter::Any,
            ..Default::default()
        }).await.unwrap().into_iter().filter(|child| matches!(child.input.as_ref(), ProcessInput::Engine { payload, .. } if payload.get("callcancel_x").is_some())).collect()
}

fn ext_tools(world: Arc<ExternalWorld>) -> Arc<dyn lash_core::ToolProvider> {
    let explicit = world.cancel_case.lock_recover().is_some();
    let fixture_policy = if explicit {
        ExecutionPolicy::repeatable(std::num::NonZeroU32::new(3).unwrap(), 10, 100)
    } else {
        ExecutionPolicy::Once
    };
    let slow = if world
        .cancel_case
        .lock_recover()
        .is_some_and(|case| case.hint.is_some())
    {
        definition(SLOW, fixture_policy, json!({ "type": "object" }))
            .with_declaration(
                lash_vocabulary::tool_declaration::ToolDeclaration::deferring()
                    .with_intents([lash_vocabulary::ToolIntentKind::StartProcess]),
                Some(lash_vocabulary::tool_contract::ParkBound::Within(
                    Duration::from_secs(120),
                )),
            )
            .unwrap()
    } else {
        definition(SLOW, fixture_policy, json!({ "type": "object" }))
            .with_declaration(
                lash_vocabulary::tool_declaration::ToolDeclaration::default()
                    .with_intents([lash_vocabulary::ToolIntentKind::StartProcess]),
                None,
            )
            .unwrap()
    };
    let object = json!({ "type": "object" });
    let repeatable = ExecutionPolicy::repeatable(std::num::NonZeroU32::new(3).unwrap(), 10, 100);
    Arc::new(StaticToolProvider::new(
        vec![
            definition(WRITE, fixture_policy, object.clone()),
            definition(RETRY, repeatable, object.clone()),
            definition(FAST, fixture_policy, object.clone()),
            slow,
            definition(
                HANDLE,
                ExecutionPolicy::Once,
                json!({ "x-lash": { "kind": "process_unknown" } }),
            ),
        ],
        ExtTools { world },
    ))
}

/// The scripted model: the cell until the transcript holds it, then prose.
/// Every request it is sent is kept, rendered.
fn model(cell: Cell, requests: Arc<Mutex<Vec<String>>>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("cell-restore-scripted")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let requests = Arc::clone(&requests);
            async move {
                requests
                    .lock_recover()
                    .push(format!("{:?}", request.messages));
                let answered = request
                    .messages
                    .iter()
                    .any(|message| message.role == lash_core::llm::types::LlmRole::Assistant);
                let answer = if answered {
                    FINAL.to_owned()
                } else {
                    cell.source()
                };
                if answered && let Some(stream) = request.stream_events.as_ref() {
                    stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                        kind: StreamBlockKind::AssistantText,
                        block: StreamBlockIdentity::new("text:0", 0),
                        text: answer.clone(),
                    }));
                }
                Ok(LlmResponse {
                    parts: vec![if answered {
                        LlmOutputPart::Text {
                            text: answer,
                            response_meta: None,
                        }
                    } else {
                        lash::testing::execute_code(answer)
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// [`sim::workers`] whose machines return to the broker every `slice`
/// charge units.
fn sliced_workers(clock: &Arc<SimClock>, slice: u64) -> lash::vm::WorkerService {
    let mut config = sim::untimed_workers().config().clone();
    config.tuning.slice = slice;
    let clock = Arc::clone(clock);
    lash::vm::WorkerService::new(config).with_call_hold(Arc::new(move || Box::new(clock.hold())))
}

// --- the awaited process ----------------------------------------------------

/// The awaited process's engine: it sleeps until its payload's `until_ms`,
/// then ends with `{ "slept": true }`; a cancel ends it cancelled.
struct SleepEngine;

fn infra(error: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(
        lash_execution_contracts::plugin_error::PluginError::Session(error.to_string()),
    )
}

#[async_trait::async_trait]
impl ProcessEngine for SleepEngine {
    fn settled_ran(
        &self,
        _kind: &lash_execution_contracts::process::EngineStepKind,
        _output: &lash_core_store::tool_run::SettledOutput,
    ) -> lash_execution_contracts::process::SettledRan {
        Default::default()
    }

    fn trace_class(&self) -> lash_execution_contracts::process::ProcessTraceClass {
        lash_execution_contracts::process::ProcessTraceClass::Internal
    }

    async fn inspect_link(
        &self,
        link: lash_kernel_value::LinkId,
        selector: &lash_execution_contracts::process::linking::ReachSelector,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Ok(lash_execution_contracts::process::linking::LinkInspection {
            link,
            document: lash_kernel_value::DocumentId::from_bytes([0; 32]),
            selector: selector.clone(),
            operations: Default::default(),
            machine: Default::default(),
            artifacts: Vec::new(),
            signature: None,
        })
    }
    async fn inspect_start(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        // This engine executes no linked code and owes no operation or machine debt.
        self.inspect_link(
            lash_kernel_value::LinkId::from_bytes([0; 32]),
            &lash_execution_contracts::process::linking::ReachSelector::Start(
                lash_kernel_value::StartSelector::Main,
            ),
        )
        .await
    }
    fn capabilities(&self) -> lash_kernel_value::claims::NodeRequirements {
        Default::default()
    }

    async fn check_args(
        &self,
        _signature: &lash_execution_contracts::process::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_execution_contracts::process::ArgsMode,
    ) -> std::result::Result<(), lash_execution_contracts::process::ArgsMismatch> {
        Err(
            lash_execution_contracts::process::ArgsMismatch::UnsupportedSignature {
                engine_kind: self.kind().into(),
            },
        )
    }

    fn kind(&self) -> &'static str {
        ENGINE
    }

    fn state_format(&self) -> EngineStateFormat {
        EngineStateFormat {
            kind: ENGINE.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn program_identity(
        &self,
        _payload: &Value,
    ) -> Option<lash_core_execution::ExecutableGeneration> {
        None
    }

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                    "process engine `{}` takes no settings",
                    self.kind()
                )),
            ),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<
        Option<lash_execution_contracts::process::EngineSettings>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(None)
    }

    fn advance(
        &self,
        state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError> {
        let script: Value = match &event {
            EngineEvent::Started { payload } => payload.clone(),
            _ => serde_json::from_slice(&state.bytes).map_err(infra)?,
        };
        let action = match event {
            EngineEvent::Started { .. } => EngineAction::Sleep {
                until: DurableInstant(script["until_ms"].as_i64().unwrap_or_default()),
                site: None,
            },
            EngineEvent::Woke => EngineAction::Terminal(ProcessOutcome::from_tool_output(
                ToolCallOutput::success(json!({ "slept": true })),
            )),
            EngineEvent::Cancelled { origin, .. } => {
                EngineAction::Terminal(ProcessOutcome::from_tool_output(ToolCallOutput::cancelled(
                    ToolCancellation::runtime("the sleep engine answered its cancel")
                        .with_origin(origin),
                )))
            }
            _ => EngineAction::Idle,
        };
        Ok((
            EngineState {
                staged: Vec::new(),
                format: self.state_format(),
                bytes: serde_json::to_vec(&script).map_err(infra)?,
                pages: None,
                requirements: Default::default(),
            },
            action,
        ))
    }

    fn start_artifacts(
        &self,
        _payload: &Value,
    ) -> Result<
        Vec<lash_core_execution::ArtifactName>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core_execution::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core_execution::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        Err(
            lash_execution_contracts::plugin_error::PluginError::Session(format!(
                "the sleep engine stores no artifact `{artifact_ref}`"
            )),
        )
    }

    async fn resolve(
        &self,
        _reference: &lash_execution_contracts::process::ProcessDefinitionRef,
    ) -> Result<
        lash_execution_contracts::process::ProcessDefinitionResolution,
        lash_execution_contracts::process::ProcessDefinitionError,
    > {
        Ok(
            lash_execution_contracts::process::ProcessDefinitionResolution::new(
                lash_execution_contracts::process::ProcessSignature::Unknown,
            ),
        )
    }
}

/// The sleep engine runs no steps.
struct NoSteps;

#[async_trait::async_trait]
impl ProcessSteps for NoSteps {
    fn stop_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    async fn admit(
        &self,
        _runtime: &Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &ProcessRecord,
        step: &StepRequest,
        _target: Option<&lash_core_execution::runtime::process::steps::InvocationTarget>,
        _now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        Err(StepRefusal::UnknownTool {
            step: step.step().0.clone(),
            tool: step.step().0.clone(),
        })
    }

    fn resolved(
        &self,
        _process: &ProcessRecord,
        _step: &StepRequest,
        _execution: &AdmittedExecution,
        _parked: &Material<lash_core_store::tool_run::CompletionSource>,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> SettledOutput {
        SettledOutput::Interrupted
    }

    fn body(
        &self,
        _runtime: &std::sync::Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &ProcessRecord,
        step: &StepRequest,
        _execution: &AdmittedExecution,
    ) -> MemberBody {
        unreachable!("no step of `{}` is ever admitted", step.step().0)
    }
}

// --- the scenario -----------------------------------------------------------

/// The scenario for one cell on one dialect, fresh for every matrix cell.
struct CellTurn {
    cell: Cell,
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<ExternalWorld>,
    requests: Arc<Mutex<Vec<String>>>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<Backend>>,
    /// The virtual clock the database was built on, which the core's VM
    /// worker calls hold.
    clock: Mutex<Option<Arc<SimClock>>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
    /// The run the host's send opened.
    run: Mutex<Option<lash_vocabulary::identity::TurnId>>,
    /// Whether the run is held to [`Self::ends_once_laws`].
    ends_once: bool,
    /// The turn a [`Cell::CancelledBeforeSave`] resolution cancels.
    cancel: Arc<CancelTarget>,
}

impl CellTurn {
    fn new(cell: Cell, dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            cell,
            dialect,
            postgres_url,
            world: Arc::default(),
            requests: Arc::default(),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            clock: Mutex::default(),
            core: Mutex::default(),
            keep: Mutex::default(),
            run: Mutex::default(),
            ends_once: false,
            cancel: Arc::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        let clock = self
            .clock
            .lock_recover()
            .clone()
            .expect("the database is built first");
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                let factory = lash::rlm::RlmProtocolPluginFactory::new(
                    lash::rlm::RlmProtocolPluginConfig::builder()
                        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                        .build(),
                    if matches!(self.cell, Cell::ExplicitCancel(case) if case.source == CancelSource::Python) {
                        lash::rlm::CellDialect::python()
                    } else {
                        lash::rlm::CellDialect::typescript()
                    },
                )
                .with_worker_service(if self.cell == Cell::LongStretch {
                    sliced_workers(&clock, STRETCH_SLICE)
                } else {
                    sim::workers(&clock)
                });
                let (deferred, tools) =
                    if matches!(self.cell, Cell::Deferred | Cell::CancelledBeforeSave) {
                        (
                            Some(Arc::new(GrantingResolver {
                                world: Arc::clone(&self.world),
                                cancel: (self.cell == Cell::CancelledBeforeSave)
                                    .then(|| Arc::clone(&self.cancel)),
                            })),
                            Arc::new(DeferredTools {
                                world: Arc::clone(&self.world),
                                listed: ext_tools(Arc::clone(&self.world)),
                            }) as Arc<dyn lash_core::ToolProvider>,
                        )
                    } else {
                        (None, ext_tools(Arc::clone(&self.world)))
                    };
                let presentation_world = Arc::clone(&self.world);
                let builder = lash::LashCore::rlm_builder(backend.clone(), factory);
                builder
                    .serve_sessions(false)
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .serve_test_llm_profile(
                        model(self.cell, Arc::clone(&self.requests)),
                        lash_core::LlmProfileMetadata::builder(MODEL)
                            .cache_retention(lash_core::provider::CacheRetention::Short)
                            .context_window_tokens(200_000)
                            .build()
                            .expect("the model's metadata"),
                    )
                    .plugin(Arc::new(lash::plugins::StaticPluginFactory::new(
                        lash::plugins::PluginDeclaration::initial("tools"),
                        match deferred {
                            Some(resolver) => packages::deferred_spec(
                                "tools",
                                lash::tool_provider_key!("deferred"),
                                "dynamic",
                                resolver,
                            ),
                            None => lash::plugins::PluginSpec::new(),
                        }
                        .with_state_reducer("increment", call_cancel::increment_reducer())
                        .with_presentation_step(lash::hook_key!("callcancel-final"), Arc::new(move |input| {
                            let world = Arc::clone(&presentation_world);
                            Box::pin(async move {
                                let selected = world.cancel_case.lock_recover().is_some_and(|case| case.order == CommitOrder::Final)
                                    && world.entries.lock_recover().get(&input.context.call_id).is_some_and(|entries|
                                        entries.iter().any(|(tool, args)| tool == SLOW && written_x(args) == Some(1)));
                                if selected {
                                    world.final_presenting.store(true, std::sync::atomic::Ordering::SeqCst);
                                    world.changed.notify_waiters();
                                    loop {
                                        let nodes = world.nodes.lock_recover().upgrade().unwrap();
                                        let trace = nodes.script().trace();
                                        let fast = world.entries().into_iter().find_map(|(id, entries)|
                                            entries.iter().any(|(tool, _)| tool == FAST).then_some(id));
                                        let fast_outcome = trace.iter().rposition(|write| {
                                            if !write.committed() { return false; }
                                            let mut decoder = lash_core_store::checkpoint_graph::GraphDecoder::new(
                                                write.facts.as_ref(), lash_core_store::checkpoint_graph::DecodeLimits::within_total(u64::MAX));
                                            write.facts.rows.iter().any(|row| {
                                                row.call == fast && matches!(
                                                    lash_core_execution::runtime::actor::round::graph::RoundFact::load(&mut decoder, row.record),
                                                    Ok(lash_core_execution::runtime::actor::round::graph::RoundFact::Outcome { .. }
                                                        | lash_core_execution::runtime::actor::round::graph::RoundFact::FinalOutcome { .. })
                                                )
                                            })
                                        });
                                        if fast_outcome.is_some() { break; }
                                        let settled = trace.iter().filter(|write| write.stored != Stored::Pending).count();
                                        nodes.script().settled(settled + 1).await;
                                    }
                                }
                                Ok(input.previous)
                            })
                        }))
                        .with_tool_provider(lash::tool_provider_key!("tools"), tools)
                        .with_package_provider(
                            lash::tool_provider_key!("tools"),
                            packages::operation_provider(
                                "tools",
                                lash::tool_provider_key!("tools"),
                                ext_tools(Arc::clone(&self.world)),
                            ),
                        ),
                    )))
                    .plugin(Arc::new(
                        lash::process_controls::SessionProcessAdminPluginFactory::new(
                            lash_execution_contracts::process::lifetime::session_or_starter,
                            lash::process::env::policy::from_run(|_| Ok(Vec::new())),
                        ),
                    ))
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        lash::persistence::LeaseOwnerId::new("cell-restore-deployment"),
                        lash::persistence::LeaseIncarnationId::new("cell-restore-boot"),
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    /// Create the session and send it the turn's input, uncut.
    async fn send(&self) -> Result<(), String> {
        let session = self
            .core()
            .session(session())
            .create(lash::SessionCreation::root(lash::SessionSpec::new(
                MODEL,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(64),
                lash_core::NoProgressBudget::bounded(12),
            )))
            .await
            .map_err(|error| format!("create the session: {error}"))?;
        let handle = session
            .send(lash::TurnInput::text("run the cell"))
            .await
            .map_err(|error| format!("send the turn's input: {error}"))?;
        if matches!(self.cell, Cell::ExplicitCancel(case) if case.source == CancelSource::Host)
            && let Some(run) = handle.id().cloned()
        {
            let host = self
                .core()
                .session(self::session())
                .durable()
                .await
                .map_err(|error| format!("resolve the cancel host: {error}"))?;
            *self.world.cancel_host.lock_recover() =
                Some((host, lash_vocabulary::identity::RunId::from_turn_id(run)));
        }
        *self.run.lock_recover() = handle.id().cloned();
        *self.cancel.turn.lock_recover() = handle.id().cloned().map(|run| (self.backend(), run));
        Ok(())
    }

    /// The model's last request, rendered: what the cell answered it.
    fn last_request(&self) -> String {
        self.requests
            .lock_recover()
            .last()
            .cloned()
            .unwrap_or_default()
    }

    /// The identity laws after a run.
    async fn identity_laws(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let entries = self.world.entries();
        let mut ids: BTreeMap<i64, Vec<&ToolCallId>> = BTreeMap::new();
        for (call, entries) in &entries {
            if entries.len() > 1 {
                violations.push(format!(
                    "call {call} reached the world {} times: {entries:?}",
                    entries.len()
                ));
            }
            for (_, args) in entries {
                let x = written_x(args).expect("a write names its x");
                ids.entry(x).or_default().push(call);
            }
        }
        // NR-2: losing a Once start may interrupt before either body runs.
        // These faults lose the owner or its acknowledgement; the uncut
        // reference and fail-before retries must populate both witnesses.
        let interrupted_once = cut.is_some_and(|cut| {
            matches!(
                cut.fault,
                Fault::Abort | Fault::CommitThenAbort | Fault::AckHidden | Fault::Zombie
            )
        });
        violations.extend(identity_population_violations(
            &ids,
            matches!(self.cell, Cell::Identity | Cell::Deferred) && !interrupted_once,
        ));
        violations.extend(turn_ended(nodes).await);
        violations
    }

    /// The `Repeatable` laws after a run: the one call re-ran at its own
    /// ordinal wherever it was cut, and the cell was answered with its
    /// value.
    async fn repeatable_laws(&self, nodes: &SimNodes) -> Vec<String> {
        let mut violations = Vec::new();
        let entries = self.world.entries();
        if entries.len() != 1 {
            violations.push(format!(
                "the one Repeatable call reached the world under {} call ids: {entries:?}",
                entries.len()
            ));
        }
        let last = self.last_request();
        if !last.contains(&format!("\\\"ran\\\": \\\"{RETRY}\\\""))
            && !last.contains(&format!("ran: \\\"{RETRY}\\\""))
            && !last.contains(RETRY)
        {
            violations.push(format!(
                "the cell was not answered with the call's value; the model saw: {last}"
            ));
        }
        for refusal in ["never answered", "interrupted"] {
            if last.contains(refusal) {
                violations.push(format!(
                    "the cell was answered `{refusal}` instead of re-running the call: {last}"
                ));
            }
        }
        violations.extend(turn_ended(nodes).await);
        violations
    }

    /// The race laws after a run: every body entry was an admitted
    /// member's; uncut, the loser settled with an outcome of its own.
    async fn race_laws(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let entered: usize = self.world.entries().values().map(Vec::len).sum();
        let admitted: usize = self.tripwire.counts().bodies.values().sum();
        if admitted < entered {
            violations.push(format!(
                "{entered} host bodies ran, but only {admitted} admitted bodies were entered: \
                 {:?}",
                self.world.entries()
            ));
        }
        if cut.is_none() {
            let outcomes = nodes
                .script()
                .trace()
                .iter()
                .filter(|write| {
                    write.point.label == CommitLabel::ROUND_OUTCOME && write.committed()
                })
                .count();
            if outcomes < 2 {
                violations.push(format!(
                    "the race's winner and loser settled in {outcomes} outcome commits"
                ));
            }
        }
        violations.extend(turn_ended(nodes).await);
        violations
    }
}

impl CellTurn {
    /// The ends-once laws after a run (FIG-5999): every body ran once, the
    /// model was asked twice in total, the turn's report holds the cell's
    /// calls once, and the race's loser, the one execution a cell leaves
    /// open at its end, was cancelled and settled.
    async fn ends_once_laws(&self, nodes: &SimNodes) -> Vec<String> {
        let mut violations = Vec::new();
        let entries = self.world.entries();
        if entries.len() != 2 || entries.values().any(|entries| entries.len() != 1) {
            violations.push(format!(
                "the cell's two bodies did not each run once: {entries:?}"
            ));
        }
        let asked = self.requests.lock_recover().len();
        if asked != 2 {
            violations.push(format!("the model was asked {asked} times, not twice"));
        }
        match self.reported_calls(nodes).await {
            Ok(reported) => {
                let unique: std::collections::BTreeSet<_> = reported.iter().collect();
                let expected = if self.cell == Cell::Race {
                    1..=2
                } else {
                    2..=2
                };
                if unique.len() != reported.len()
                    || !expected.contains(&reported.len())
                    || reported.iter().any(|call| !entries.contains_key(call))
                {
                    violations.push(format!(
                        "the turn's report does not hold the cell's calls once: {reported:?}, of the bodies {:?}",
                        entries.keys().collect::<Vec<_>>()
                    ));
                }
            }
            Err(error) => violations.push(error),
        }
        if self.cell == Cell::Race {
            let cancelled = self
                .world
                .cancelled
                .load(std::sync::atomic::Ordering::SeqCst);
            let settled = nodes
                .script()
                .trace()
                .iter()
                .filter(|write| {
                    write.point.label == CommitLabel::ROUND_OUTCOME && write.committed()
                })
                .count();
            if cancelled != 1 || settled < 2 {
                violations.push(format!(
                    "the cell left its race's loser open: {cancelled} bodies were cancelled, and {settled} outcomes committed"
                ));
            }
        }
        violations
    }

    /// The calls the ended turn's report holds of its cell, in order: the
    /// cell reports its transcript names (FIG-6601).
    async fn reported_calls(&self, nodes: &SimNodes) -> Result<Vec<ToolCallId>, String> {
        use lash_core_execution::runtime::actor::round;
        use lash_core_store::checkpoint_graph::{DecodeLimits, GraphDecoder, GraphRefusal};

        let run = self
            .run
            .lock_recover()
            .clone()
            .ok_or("the host's send opened no run")?;
        let transcript = match nodes
            .database()
            .hydrate(
                lash_durable::domain::RootOwner::RunTranscript {
                    session: session(),
                    run,
                },
                None,
                DecodeLimits::within_total(u64::MAX),
            )
            .await
        {
            Ok(transcript) => transcript,
            Err(lash_durable::DurableError::Graph(GraphRefusal::RootRetired)) => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(format!("the turn's transcript: {error}")),
        };
        let mut decoder = GraphDecoder::new(&transcript, DecodeLimits::within_total(u64::MAX));
        let mut calls = Vec::new();
        let entries = round::TurnTranscript::load(&mut decoder, transcript.root)
            .map_err(|error| format!("the turn's transcript: {error}"))?
            .entries;
        for entry in entries {
            let round::TranscriptEntry::Report(report) = entry else {
                continue;
            };
            let report = round::graph::ReportView::load(&mut decoder, report)
                .map_err(|error| format!("the cell's report: {error}"))?;
            for body in report.entries {
                let body = round::graph::CallBody::load(&mut decoder, body)
                    .map_err(|error| format!("the call's body: {error}"))?;
                calls.push(body.record.call_id);
            }
        }
        Ok(calls)
    }
}

impl CellTurn {
    /// A cancel accepted before the cell's first save admits nothing
    /// (FIG-6000): no body was entered, and the turn ended cancelled.
    async fn cancelled_before_save_laws(&self, nodes: &SimNodes) -> Vec<String> {
        let mut violations = Vec::new();
        if self
            .world
            .resolved
            .load(std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            violations.push("the cell never resolved its call: no cancel was requested".to_owned());
        }
        let entries = self.world.entries();
        if !entries.is_empty() {
            violations.push(format!(
                "a cancel accepted before the cell's first save let bodies run: {entries:?}"
            ));
        }
        let run = self.run.lock_recover().clone();
        match run {
            Some(run) => match nodes.database().turn_end(&session(), &run).await {
                Ok(Some(end))
                    if end.kind() == lash_core_store::store::RunTerminalKind::Cancelled => {}
                other => violations.push(format!("the cancelled turn's end is {other:?}")),
            },
            None => violations.push("the host's send opened no run".to_owned()),
        }
        violations.extend(turn_ended(nodes).await);
        violations
    }

    /// A response held past its first slice is recorded (FIG-6000): a node
    /// lost after the cell's first stretch outlasted a slice, before the
    /// stretch's save, leaves the cell to its successor, which sends no
    /// provider request for it. The model is asked once for the cell and
    /// once for the answer, and the body runs once.
    async fn held_past_its_slice_laws(&self, nodes: &SimNodes) -> Vec<String> {
        let mut violations = Vec::new();
        let asked = self.requests.lock_recover().len();
        if asked != 2 {
            violations.push(format!(
                "the model was asked {asked} times, not once for the cell and once after it"
            ));
        }
        let entries = self.world.entries();
        if entries.len() != 1 || entries.values().any(|entries| entries.len() != 1) {
            violations.push(format!("the cell's body did not run once: {entries:?}"));
        }
        violations.extend(turn_ended(nodes).await);
        violations
    }
}

/// A comparison needs both body populations as witnesses.
fn identity_population_violations(
    ids: &BTreeMap<i64, Vec<&ToolCallId>>,
    require_populations: bool,
) -> Vec<String> {
    if require_populations {
        let missing: Vec<String> = [(1, "A"), (2, "B")]
            .into_iter()
            .filter(|(x, _)| ids.get(x).is_none_or(Vec::is_empty))
            .map(|(_, operation)| format!("operation {operation} has no body identity witness"))
            .collect();
        if !missing.is_empty() {
            return missing;
        }
    }
    if let (Some(a), Some(b)) = (ids.get(&1), ids.get(&2))
        && a.iter().any(|call| b.contains(call))
    {
        return vec!["operation B took operation A's ToolCallId".to_owned()];
    }
    Vec::new()
}

#[test]
fn identity_comparison_refuses_a_missing_body_population() {
    let call = ToolCallId::fixture("body-witness");
    for (present, missing) in [(1, "B"), (2, "A")] {
        let ids = BTreeMap::from([(present, vec![&call])]);
        assert_eq!(
            identity_population_violations(&ids, true),
            vec![format!("operation {missing} has no body identity witness")],
        );
    }
}

/// The turn ended, and nothing stays bound to it.
async fn turn_ended(nodes: &SimNodes) -> Vec<String> {
    let mut violations = Vec::new();
    match nodes.database().turn(&session()).await {
        Ok(None) => {}
        other => violations.push(format!("the turn did not end: {other:?}")),
    }
    match nodes.database().session_mailbox(&session()).await {
        Ok(mailbox) if mailbox.bound_run.is_none() => {}
        other => violations.push(format!("the ended run still holds its rows: {other:?}")),
    }
    violations
}

/// The runtime core's backend over `stores`, in the shipped build's format
/// sets, with the awaited process's engine.
fn shipped_backend(stores: Arc<dyn StoreSet>) -> Backend {
    Backend::assemble(BackendParts {
        stores,
        settings: sim::settings(),
        engines: vec![
            Arc::new(SleepEngine),
            Arc::new(lash_core_execution::testing::HeldProcessEngine),
        ],

        formats: lash::formats::actor_state_surfaces(),
    })
    .expect("the backend assembles")
}

#[async_trait::async_trait]
impl Scenario for CellTurn {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        *self.clock.lock_recover() = Some(Arc::clone(&clock));
        let (stores, database) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        lash_execution_contracts::testing::process_execution_env_fixture(
            stores.process_env_store().as_ref(),
        )
        .await;
        let backend = shipped_backend(stores);
        if let Cell::ExplicitCancel(case) = self.cell {
            *self.world.cancel_case.lock_recover() = Some(case);
            *self.world.cancel_backend.lock_recover() = Some(backend.clone());
        }
        *self.backend.lock_recover() = Some(backend);
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: self.backend().formats().decodes(),
            serves: lash::testing::node_serves(&self.core()),
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        Arc::new(ActorDispatch {
            session: Arc::new(SessionActivation::new(
                self.backend(),
                lash::testing::session_turn_services(&self.core()),
                Arc::clone(&self.tripwire) as _,
            )),
            process: Arc::new(ProcessActivation::new(
                self.backend(),
                Arc::new(NoSteps),
                Arc::clone(&self.tripwire) as _,
            )),
        })
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        *self.world.nodes.lock_recover() = Arc::downgrade(nodes);
        self.send().await?;
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![actor()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        let done = matches!(
            nodes.database().actor(&actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
        );
        if done {
            self.world.cancel_host.lock_recover().take();
        }
        done
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = match self.cell {
            Cell::Repeatable => self.repeatable_laws(nodes).await,
            Cell::Race => self.race_laws(nodes, cut).await,
            Cell::ExplicitCancel(case) => self.cancellation_laws(nodes, cut, case).await,
            Cell::Identity | Cell::Sleep | Cell::AwaitProcess | Cell::Deferred => {
                self.identity_laws(nodes, cut).await
            }
            Cell::CancelledBeforeSave => self.cancelled_before_save_laws(nodes).await,
            Cell::LongStretch => self.held_past_its_slice_laws(nodes).await,
        };
        match self
            .backend()
            .stores()
            .execution_resolutions()
            .requirements(&actor())
            .await
        {
            Ok(requirements) if requirements.is_empty() => {}
            other => violations.push(format!(
                "a terminal turn retains cell execution claim debt: {other:?}"
            )),
        }
        if self.ends_once {
            violations.extend(self.ends_once_laws(nodes).await);
        }
        violations
    }
}

/// Kill the owner of `cell` between the cell's end and the commit that
/// consumes its answer: at each `model.start`, before it enters the store.
/// The first is the turn's own call for the cell; the second consumes the
/// cell's answer.
async fn ends_once(cell: Cell, dialect: Dialect, postgres_url: Option<String>) {
    let report = Matrix::new()
        .labels(&[CommitLabel::MODEL_START])
        .faults(&[Fault::Abort])
        .horizon(Duration::from_secs(600))
        .run_test(|| CellTurn {
            ends_once: true,
            ..CellTurn::new(cell, dialect, postgres_url.clone())
        })
        .await;
    eprintln!(
        "{cell:?} ends once on {dialect:?}: {} cells over {} labels",
        report.cells.len(),
        report.labels().len()
    );
    report.assert_held();
    assert!(
        report.cells.len() >= 2,
        "{cell:?}: the matrix cut {} model starts, not the one that consumes the cell",
        report.cells.len()
    );
}

/// A cancel accepted before a cell's first save admits nothing (FIG-6000):
/// the cell's deferred resolution, which runs before that save, has the host
/// cancel the turn `Immediate`.
async fn cancelled_before_save(dialect: Dialect) {
    let report = Matrix::new()
        .faults(&[])
        .run_test(|| CellTurn::new(Cell::CancelledBeforeSave, dialect, None))
        .await;
    report.assert_held();
}

/// A response held past its first slice is recorded (FIG-6000): kill the
/// owner of a cell whose first stretch outlasts its slice at the stretch's
/// save, before it enters the store.
async fn held_past_its_slice(dialect: Dialect) {
    let report = Matrix::new()
        .labels(&[
            CommitLabel::CELL_SNAPSHOT_ADMIT,
            CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT,
        ])
        .faults(&[Fault::Abort])
        .horizon(Duration::from_secs(600))
        .run_test(|| CellTurn::new(Cell::LongStretch, dialect, None))
        .await;
    eprintln!(
        "a held response past its slice on {dialect:?}: {} cells over {:?}",
        report.cells.len(),
        report.labels()
    );
    report.assert_held();
    assert!(
        !report.cells.is_empty(),
        "the matrix cut no save of the cell's first stretch"
    );
}

/// Cut `cell` at every labelled write under every fault.
async fn matrix(cell: Cell, dialect: Dialect, postgres_url: Option<String>) {
    let faults = [
        Fault::FailBefore,
        Fault::AckHidden,
        Fault::Zombie,
        Fault::Abort,
        Fault::CommitThenAbort,
    ];
    // Split only the optional double-run proof into exhaustive fault batches
    // so each action retains the existing timeout. Ordinary laws cut all five.
    let selected = if std::env::var("LASH_MATRIX_VERIFY_LEASE").as_deref() == Ok("1") {
        match std::env::var("LASH_MATRIX_PROOF_PART").as_deref() {
            Ok("first") => &faults[..3],
            Ok("second") => &faults[3..],
            _ => &faults[..],
        }
    } else {
        &faults[..]
    };
    let report = Matrix::new()
        .faults(selected)
        .horizon(Duration::from_secs(600))
        .run_test(|| CellTurn::new(cell, dialect, postgres_url.clone()))
        .await;
    eprintln!(
        "{cell:?} on {dialect:?}: {} cells over {} labels",
        report.cells.len(),
        report.labels().len()
    );
    report.assert_held();
}

/// Identity: cut at every labelled write under every fault, A's and B's
/// `ToolCallId`s differ.
async fn identity(dialect: Dialect, postgres_url: Option<String>) {
    matrix(Cell::Identity, dialect, postgres_url).await;
}

/// Run `turn` on fresh simulated nodes over its database.
async fn simulated(turn: &CellTurn) -> (Arc<SimClock>, Arc<dyn DurableStore>, SimNodes) {
    let clock = SimClock::new();
    let database = turn.database(Arc::clone(&clock)).await;
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        turn.config(),
        turn.activation(),
    );
    for node in ["a", "b", "c"] {
        nodes.offer_on(node, lash::testing::node_capabilities(&turn.core()));
    }
    (clock, database, nodes)
}

/// Sleep: the cell sleeps for `SLEEP_MS`. Its session stays hot for the
/// idle eviction, then releases as `waiting` with the sleep's deadline as
/// its next due, holding nothing while its timer row persists (ADR 0132
/// §6). Node A is killed during the sleep and node B started; nothing
/// claims the session before the deadline, and B wakes the cell at the
/// deadline it was admitted with, once.
async fn sleep_across_a_crash(dialect: Dialect, postgres_url: Option<String>) {
    let turn = CellTurn::new(Cell::Sleep, dialect, postgres_url);
    let (clock, database, nodes) = simulated(&turn).await;
    turn.send().await.expect("the turn is sent");
    nodes.start("a");
    // Run A until the cell sleeps on its pinned timer.
    let timer = loop {
        let timers: Vec<_> = database
            .pending_waits(&actor())
            .await
            .expect("the session's waits read")
            .into_iter()
            .filter(|wait| wait.purpose.kind() == WaitKind::Timer)
            .collect();
        if let [timer] = timers.as_slice() {
            break timer.clone();
        }
        assert!(
            nodes.step().await.is_some() && clock.logical_ms() < SLEEP_MS,
            "the cell never slept on one timer: {timers:?}\n{}",
            nodes.script().rendered_trace()
        );
    };
    let due = timer.purpose.deadline().expect("a timer has a deadline");
    let deadline = u64::try_from(due.0).unwrap() - SimClock::timestamp_ms_at(0);
    let slept_at = clock.logical_ms();
    assert!(
        deadline >= slept_at + SLEEP_MS - 1_000 && deadline <= slept_at + SLEEP_MS,
        "the timer is due at {deadline}, not {SLEEP_MS} ms after the cell slept at {slept_at}"
    );
    // The idle session releases as `waiting` until the sleep's deadline,
    // owned by nobody, while its timer stays pending.
    let half = slept_at + SLEEP_MS / 2;
    let evicted = loop {
        let session = database.actor(&actor()).await.expect("read the session");
        if let Some(session) = &session
            && session.state == ActorState::Waiting
        {
            break session.clone();
        }
        assert!(
            nodes.step().await.is_some() && clock.logical_ms() < half,
            "the sleeping cell's session never released as `waiting` by {half} ms: {session:?}\n{}",
            nodes.script().rendered_trace()
        );
    };
    let mut violations = Vec::new();
    if evicted.owner.is_some() || evicted.next_due != Some(due) {
        violations.push(format!(
            "the evicted session holds {:?} and is next due at {:?}, not nobody at {due:?}",
            evicted.owner, evicted.next_due
        ));
    }
    let pending = database
        .pending_waits(&actor())
        .await
        .expect("the session's waits read");
    if !pending.iter().any(|wait| wait.id == timer.id) {
        violations.push(format!(
            "the evicted session's timer does not persist: {pending:?}"
        ));
    }
    let released_at = clock.logical_ms();
    // Kill A during the sleep, and let B take the cell over.
    while clock.logical_ms() < half {
        assert!(nodes.step().await.is_some(), "A stalled while sleeping");
    }
    nodes.kill("a");
    nodes.quiesce().await;
    nodes.start("b");
    let horizon = deadline + 2 * SLEEP_MS;
    while !turn.done(&nodes).await {
        assert!(
            clock.logical_ms() < horizon,
            "the turn is not done {} ms after its sleep's deadline:\n{}",
            clock.logical_ms() - deadline,
            nodes.script().rendered_trace()
        );
        assert!(
            nodes.step().await.is_some(),
            "B stalled:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;
    violations.extend(turn_ended(&nodes).await);
    let trace = nodes.script().trace();
    // Nobody claims the released session before its deadline.
    let early: Vec<_> = trace
        .iter()
        .filter(|write| {
            write.point.label == CommitLabel::CLAIM
                && matches!(write.stored, Stored::Committed { effective: true })
                && write.at_ms > released_at
                && write.at_ms < deadline
        })
        .map(|write| (write.node.to_string(), write.at_ms))
        .collect();
    if !early.is_empty() {
        violations.push(format!(
            "the session released at {released_at} was claimed before its deadline \
             {deadline}: {early:?}"
        ));
    }
    // The cell woke when B committed the model call that consumes its
    // answer, and with it the cell's end (FIG-5999): at its admitted
    // deadline, within a claim poll of it. A sleep run again from its
    // recovery would end `SLEEP_MS` after B took the cell over.
    let woke = trace
        .iter()
        .find(|write| {
            &*write.node == "b"
                && write.point.label == CommitLabel::MODEL_START
                && write.committed()
        })
        .map(|write| write.at_ms);
    let poll = Matrix::test_lease().settings().claim_poll.as_millis() as u64;
    if !woke.is_some_and(|woke| woke >= deadline && woke <= deadline + poll + 5_000) {
        violations.push(format!(
            "the cell slept at {slept_at}, due at {deadline}, was released at {released_at}, \
             A was killed at {half}, and the cell woke at {woke:?}"
        ));
    }
    // The cell resumed once: the model was asked for the cell, then once
    // more with its answer.
    let requests = turn.requests.lock_recover().len();
    if requests != 2 {
        violations.push(format!(
            "the model was asked {requests} times, not once for the cell and once after it woke"
        ));
    }
    assert!(
        violations.is_empty(),
        "sleep across a crash on {dialect:?}:\n  {}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
}

/// DEFER1: a cell's deferred tool resolutions are held in its snapshot and
/// restored from it. The cell calls `tools.ext_granted`, which no catalog
/// lists and the deployment's resolver grants, then sleeps. Node A is
/// killed during the sleep and the deployment is redeployed: its resolver
/// now grants nothing, and its catalog binds another tool at that path.
/// Node B restores the cell onto its sleep; the cell's second call runs
/// under the grant it resolved, never the changed ambient binding, and the
/// resolver is never asked again.
async fn deferred_grant_across_a_crash(dialect: Dialect, postgres_url: Option<String>) {
    use std::sync::atomic::Ordering;
    let turn = CellTurn::new(Cell::Deferred, dialect, postgres_url);
    *turn.world.binding.lock_recover() = Some(lash_kernel_value::providers::FormatVersion::ONE);
    let (clock, database, nodes) = simulated(&turn).await;
    turn.send().await.expect("the turn is sent");
    nodes.start("a");
    // Run A until the cell sleeps on its pinned timer.
    loop {
        let timers = database
            .pending_waits(&actor())
            .await
            .expect("the session's waits read")
            .into_iter()
            .filter(|wait| wait.purpose.kind() == WaitKind::Timer)
            .count();
        if timers == 1 {
            break;
        }
        assert!(
            nodes.step().await.is_some() && clock.logical_ms() < SLEEP_MS,
            "the cell never slept: {}\n{}",
            turn.last_request(),
            nodes.script().rendered_trace()
        );
    }
    let slept_at = clock.logical_ms();
    while clock.logical_ms() < slept_at + SLEEP_MS / 2 {
        assert!(nodes.step().await.is_some(), "A stalled while sleeping");
    }
    nodes.kill("a");
    nodes.quiesce().await;
    turn.world.redeployed.store(true, Ordering::SeqCst);
    nodes.start("b");
    let horizon = slept_at + 3 * SLEEP_MS;
    while !turn.done(&nodes).await {
        assert!(
            clock.logical_ms() < horizon,
            "the turn is not done:\n{}",
            nodes.script().rendered_trace()
        );
        assert!(
            nodes.step().await.is_some(),
            "B stalled:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;
    let mut violations = turn.identity_laws(&nodes, None).await;
    let calls = turn
        .world
        .entries()
        .into_values()
        .flatten()
        .map(|(name, args)| (name, written_x(&args).unwrap_or_default()))
        .collect::<std::collections::BTreeSet<_>>();
    let expected = [(GRANTED.to_owned(), 1), (GRANTED.to_owned(), 2)]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    if calls != expected {
        violations.push(format!(
            "the restored cell's calls ran as {calls:?}, not both under the grant it resolved"
        ));
    }
    let resolved = turn.world.resolved.load(Ordering::SeqCst);
    if resolved != 3 {
        violations.push(format!(
            "L-DEFERRED-PIN: the resolver was asked {resolved} times, not once per operation use"
        ));
    }
    let last = turn.last_request();
    if !last.contains(GRANTED) || last.contains(AMBIENT) {
        violations.push(format!(
            "the cell was not answered by the granted tool; the model saw: {last}"
        ));
    }
    // The refusal and the unavailable decision are matches too: the
    // restored cell's second calls meet the recorded decisions, never the
    // redeployed resolver's.
    // The cell's logged output, before the prompt lists its variables.
    let logged = last.split_once("Output:").map_or("", |(_, rest)| {
        rest.split("Available variables").next().unwrap_or(rest)
    });
    for recorded in [REFUSED_REASON, MISSING_NOTE] {
        let seen = logged.matches(recorded).count();
        if seen != 2 {
            violations.push(format!(
                "the cell met `{recorded}` {seen} times, not before and after the crash; the model saw: {last}"
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "deferred grant across a crash on {dialect:?}:\n  {}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
}

/// OWED-IS-NOT-INSTALLED for a committed match (FIG-6089; FIG-6075 R-4):
/// a parked cell owes the provider and codec of every deferred match it
/// committed, so only a node that serves them claims its session. The cell
/// calls `tools.ext_granted`, which the resolver matches to the `tools`
/// provider with a binding that provider reads, and the call settles; the
/// cell then sleeps before calling it again. While it sleeps, the
/// session's requirements still name the matched provider, though no call
/// to it is admitted, and the codec version its binding is in.
async fn a_parked_cells_committed_match_is_required(dialect: Dialect) {
    let turn = CellTurn::new(Cell::Deferred, dialect, None);
    *turn.world.binding.lock_recover() = Some(lash_kernel_value::providers::FormatVersion::ONE);
    let (clock, database, nodes) = simulated(&turn).await;
    let written = Arc::new(Mutex::new(None::<Vec<String>>));
    nodes.script().observe_commits({
        let written = Arc::clone(&written);
        Arc::new(move |_, tx| {
            if tx.actor() == &actor()
                && let Some(requires) = tx.requires()
            {
                *written.lock_recover() = Some(requires.to_vec());
            }
        })
    });
    turn.send().await.expect("the turn is sent");
    nodes.start("a");
    loop {
        let timers = database
            .pending_waits(&actor())
            .await
            .expect("the session's waits read")
            .into_iter()
            .filter(|wait| wait.purpose.kind() == WaitKind::Timer)
            .count();
        if timers == 1 {
            break;
        }
        assert!(
            nodes.step().await.is_some() && clock.logical_ms() < SLEEP_MS,
            "the cell never slept: {}\n{}",
            turn.last_request(),
            nodes.script().rendered_trace()
        );
    }
    nodes.kill("a");
    nodes.quiesce().await;
    let granted_call = turn
        .world
        .entries()
        .into_values()
        .flatten()
        .filter(|(name, _)| name == GRANTED)
        .count();
    assert_eq!(
        granted_call, 1,
        "the granted call ran once before the sleep"
    );
    let matched = lash_vocabulary::claims::ObligationToken::for_callback(
        lash_vocabulary::claims::RegistryKind::Session,
        &lash::plugins::ToolProviderHandle::new(
            lash_vocabulary::claims::RegistryKind::Session,
            "tools",
            lash::tool_provider_key!("tools"),
        )
        .callback_identity(),
    )
    .to_string();
    let requires = written.lock_recover().clone().unwrap_or_default();
    assert!(
        requires.contains(&matched),
        "the sleeping cell's committed match keeps `{matched}` required: {requires:?}\n{}",
        nodes.script().rendered_trace()
    );
    // DYNAMIC-MATCH across a crash (FIG-6993): the match introduced the
    // codec version of its binding, which the session's record names while
    // the cell sleeps. Past the deadline, a node offering everything else
    // does not take the session over; a node that offers the codec does,
    // and the cell ends there, its first call's outcome reused.
    let codecs = ObligationToken::requirements_of(requires.iter().map(String::as_str)).codecs;
    assert!(
        !codecs.is_empty(),
        "the committed match's codec version is required: {requires:?}"
    );
    // Past the sleep's deadline and the dead node's lease.
    clock
        .advance_to(clock.logical_ms() + SLEEP_MS + 1_000)
        .await;
    let lacking: Vec<String> = turn
        .config()
        .serves
        .into_iter()
        .filter(|served| {
            ObligationToken::requirements_of([served.as_str()])
                .codecs
                .is_disjoint(&codecs)
        })
        .collect();
    let mut offers = lash::testing::node_capabilities(&turn.core());
    assert!(codecs.is_subset(&offers.codecs));
    offers.codecs.retain(|codec| !codecs.contains(codec));
    nodes.offer_on("b", offers);
    nodes.serve_on("b", lacking);
    nodes.start("b");
    let horizon = clock.logical_ms() + SLEEP_MS;
    while clock.logical_ms() < horizon && nodes.step().await.is_some() {}
    nodes.quiesce().await;
    assert!(
        !turn.done(&nodes).await,
        "a node lacking the matched codec version took the session over:\n{}",
        nodes.script().rendered_trace()
    );
    nodes.kill("b");
    nodes.quiesce().await;
    nodes.start("c");
    let horizon = clock.logical_ms() + 3 * SLEEP_MS;
    while !turn.done(&nodes).await {
        assert!(
            clock.logical_ms() < horizon && nodes.step().await.is_some(),
            "the node offering the codec did not end the turn:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;
    assert_eq!(turn_ended(&nodes).await, Vec::<String>::new());
    let granted_calls = turn
        .world
        .entries()
        .into_values()
        .flatten()
        .filter(|(name, _)| name == GRANTED)
        .count();
    assert_eq!(
        granted_calls, 2,
        "the call before the sleep ran once and its outcome was reused; the one after it ran on C"
    );
}

/// BINDING-FORMAT-REFUSES (law 14; S8): the resolver grants
/// `tools.ext_granted` with an execution binding in format 3, which its
/// codec provider does not declare it reads. The cell parks, typed, before
/// the match is offered: the provider's reader never sees the binding and
/// no body runs. (A grant in format 1, an older format the provider still
/// reads, runs: [`deferred_grant_across_a_crash`].)
async fn an_unreadable_binding_parks_before_its_provider_reads_it(dialect: Dialect) {
    let turn = CellTurn::new(Cell::Deferred, dialect, None);
    *turn.world.binding.lock_recover() = lash_kernel_value::providers::FormatVersion::new(3);
    let (clock, database, nodes) = simulated(&turn).await;
    turn.send().await.expect("the turn is sent");
    nodes.start("a");
    let horizon = clock.logical_ms() + 3 * SLEEP_MS;
    while !matches!(
        database.actor(&actor()).await,
        Ok(Some(row)) if row.state == ActorState::Parked
    ) {
        assert!(
            clock.logical_ms() < horizon && nodes.step().await.is_some(),
            "the session never parked:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;
    let park = format!(
        "{:?}",
        turn.core()
            .session(session())
            .durable()
            .await
            .expect("the session resolves")
            .park_reason()
            .await
            .expect("the park reads")
    );
    assert!(
        park.contains("ExecutionBindingUnreadable"),
        "the session parks on the unreadable binding: {park}"
    );
    assert!(
        turn.world.entries().is_empty(),
        "no body ran: {:?}",
        turn.world.entries()
    );
}

#[tokio::test]
async fn an_unreadable_binding_parks_before_its_provider_reads_it_on_sqlite_memory() {
    an_unreadable_binding_parks_before_its_provider_reads_it(Dialect::SqliteMemory).await;
}

/// Run `turn`, a cell awaiting a host process that sleeps [`PROCESS_MS`],
/// until its `processes.await` parks on its completion wait and the session
/// releases as `waiting`: the nodes, the process, and whether it released
/// before the process ended.
async fn parked_on_a_process(
    turn: &CellTurn,
) -> (
    Arc<SimClock>,
    Arc<dyn DurableStore>,
    SimNodes,
    ProcessId,
    bool,
) {
    let (clock, database, nodes) = simulated(turn).await;
    let until = SimClock::timestamp_ms_at(0) + PROCESS_MS;
    let process = turn
        .backend()
        .process_registry()
        .register_process(ProcessRegistration::new(
            ProcessInput::Engine {
                kind: ENGINE.to_owned(),
                payload: json!({ "until_ms": until }),
                launch: lash_execution_contracts::process::EngineLaunch::new(
                    lash_execution_contracts::testing::process_execution_env_fixture_ref(),
                    None,
                ),
            },
            ProcessProvenance::host(),
            LifetimeDecision::Detached,
        ))
        .await
        .expect("register the awaited process")
        .id;
    *turn.world.process.lock_recover() = Some(process.clone());
    turn.send().await.expect("the turn is sent");
    nodes.start("a");
    // The session releases as `waiting` while the process still sleeps.
    let mut released = false;
    while clock.logical_ms() < PROCESS_MS {
        nodes.quiesce().await;
        let session = database.actor(&actor()).await.expect("read the session");
        let parked = database
            .pending_waits(&actor())
            .await
            .expect("the session's waits read")
            .iter()
            .any(|wait| wait.purpose.kind() == WaitKind::ToolCompletion);
        if parked && session.is_some_and(|snapshot| snapshot.state == ActorState::Waiting) {
            released = true;
            break;
        }
        if nodes.step().await.is_none() {
            clock.advance_by(1_000).await;
        }
    }
    (clock, database, nodes, process, released)
}

/// Await process: the cell's `processes.await` parks on its completion
/// wait, the session releases as `waiting` while the process sleeps, and
/// the cell resumes with the process's outcome once it ends.
async fn await_process(dialect: Dialect, postgres_url: Option<String>) {
    let turn = CellTurn::new(Cell::AwaitProcess, dialect, postgres_url);
    let (clock, database, nodes, process, released) = parked_on_a_process(&turn).await;
    let process_actor = ActorKey::process(process.as_str()).expect("a process actor key");
    let mut violations = Vec::new();
    if !released {
        violations.push(format!(
            "the session never released as `waiting` on its parked cell while the process \
             slept; the model saw: {}",
            turn.last_request()
        ));
    }
    // The process ends; the cell resumes with its outcome.
    let horizon = PROCESS_MS * 2;
    while released && !turn.done(&nodes).await {
        if clock.logical_ms() >= horizon {
            violations.push(format!(
                "the turn is not done {} ms after the process's end",
                clock.logical_ms() - PROCESS_MS
            ));
            break;
        }
        if nodes.step().await.is_none() {
            clock.advance_by(1_000).await;
        }
    }
    nodes.quiesce().await;
    if released {
        match database.actor(&process_actor).await {
            Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal => {}
            other => violations.push(format!("the process did not end: {other:?}")),
        }
        let last = turn.last_request();
        if !last.contains("slept") {
            violations.push(format!(
                "the cell was not resumed with the process's outcome; the model saw: {last}"
            ));
        }
        violations.extend(turn_ended(&nodes).await);
    }
    assert!(
        violations.is_empty(),
        "await process on {dialect:?}:\n  {}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
}

/// After-step during a process await: an `AfterStep` request committed
/// while the cell's `processes.await` is parked never cancels the awaited
/// process or cuts its wait. The process ends on its own terms, the cell
/// resumes with its outcome and the step closes, and the turn stops at that
/// boundary: it ends no sooner than the process, with no model call after
/// the step.
async fn after_step_during_a_process_await(dialect: Dialect, postgres_url: Option<String>) {
    let turn = CellTurn::new(Cell::AwaitProcess, dialect, postgres_url);
    let (clock, database, nodes, process, released) = parked_on_a_process(&turn).await;
    assert!(
        released,
        "the session never released on its parked cell:\n{}",
        nodes.script().rendered_trace()
    );
    let run = database
        .turn(&session())
        .await
        .expect("the turn reads")
        .expect("the parked turn is open")
        .run;
    let answer = lash_core::runtime::durable::session::request_turn_cancel(
        &turn.backend(),
        lash_durable::domain::TurnCancelRequest {
            session: session(),
            run,
            request_id: "stop-in-process".to_owned(),
            origin: None,
            reason: None,
            undelivered: lash_core::TurnCancelUndeliveredInputPolicy::Defer,
            mode: lash_core::TurnCancelMode::AfterStep,
        },
    )
    .await
    .expect("the request commits");
    assert_eq!(answer, lash_durable::domain::TurnCancelAnswer::Requested);
    let horizon = PROCESS_MS * 2;
    let mut violations = Vec::new();
    while !turn.done(&nodes).await {
        if clock.logical_ms() < PROCESS_MS
            && !matches!(database.turn(&session()).await, Ok(Some(_)))
        {
            violations.push(format!(
                "the after-step stop ended the turn at {} ms, before the process's end",
                clock.logical_ms()
            ));
            break;
        }
        if clock.logical_ms() >= horizon {
            violations.push(format!(
                "the turn is not done {} ms after the process's end",
                clock.logical_ms() - PROCESS_MS
            ));
            break;
        }
        if nodes.step().await.is_none() {
            clock.advance_by(1_000).await;
        }
    }
    nodes.quiesce().await;
    let record = turn
        .backend()
        .process_registry()
        .get_process(&process)
        .await
        .expect("the process reads")
        .expect("the process is retained");
    let terminal = format!("{:?}", record.terminal());
    if !terminal.contains("slept") || terminal.contains("Cancel") {
        violations.push(format!(
            "the awaited process did not end on its own terms: {terminal}"
        ));
    }
    let requests = turn.requests.lock_recover().len();
    if requests != 1 {
        violations.push(format!(
            "the model was called {requests} times; an after-step stop calls it no more after \
             the step"
        ));
    }
    violations.extend(turn_ended(&nodes).await);
    assert!(
        violations.is_empty(),
        "after-step during a process await on {dialect:?}:\n  {}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
}

/// A restored cell never reuses a `ToolCallId`, on SQLite in memory.
#[tokio::test]
async fn a_restored_cell_never_reuses_a_tool_call_id_on_sqlite_memory() {
    identity(Dialect::SqliteMemory, None).await;
}

/// The same law on a SQLite file.
#[tokio::test]
async fn a_restored_cell_never_reuses_a_tool_call_id_on_sqlite_file() {
    identity(Dialect::SqliteFile, None).await;
}

/// The same law on PostgreSQL.
#[tokio::test]
async fn a_restored_cell_never_reuses_a_tool_call_id_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    identity(Dialect::Postgres, Some(url)).await;
}

/// A sleeping cell's session suspends on idle eviction, and the cell wakes
/// once at its admitted deadline across a crash, on SQLite in memory.
#[tokio::test]
async fn a_sleeping_cell_suspends_on_idle_eviction_and_wakes_once_at_its_deadline_on_sqlite_memory()
{
    sleep_across_a_crash(Dialect::SqliteMemory, None).await;
}

/// The same law on a SQLite file.
#[tokio::test]
async fn a_sleeping_cell_suspends_on_idle_eviction_and_wakes_once_at_its_deadline_on_sqlite_file() {
    sleep_across_a_crash(Dialect::SqliteFile, None).await;
}

/// The same law on PostgreSQL.
#[tokio::test]
async fn a_sleeping_cell_suspends_on_idle_eviction_and_wakes_once_at_its_deadline_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    sleep_across_a_crash(Dialect::Postgres, Some(url)).await;
}

/// A `Repeatable` call in a cell, cut at its admission or at its lost
/// outcome commit, re-runs at the same ordinal, on SQLite in memory.
#[tokio::test]
async fn a_repeatable_call_in_a_cell_reruns_at_its_ordinal_on_sqlite_memory() {
    matrix(Cell::Repeatable, Dialect::SqliteMemory, None).await;
}

/// The same law on a SQLite file.
#[tokio::test]
async fn a_repeatable_call_in_a_cell_reruns_at_its_ordinal_on_sqlite_file() {
    matrix(Cell::Repeatable, Dialect::SqliteFile, None).await;
}

/// The same law on PostgreSQL.
#[tokio::test]
async fn a_repeatable_call_in_a_cell_reruns_at_its_ordinal_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    matrix(Cell::Repeatable, Dialect::Postgres, Some(url)).await;
}

/// A race loser in a cell is admitted and settles on its own; nothing runs
/// unadmitted, on SQLite in memory.
#[tokio::test]
async fn a_race_loser_in_a_cell_is_admitted_and_settles_on_sqlite_memory() {
    matrix(Cell::Race, Dialect::SqliteMemory, None).await;
}

/// The same law on a SQLite file.
#[tokio::test]
async fn a_race_loser_in_a_cell_is_admitted_and_settles_on_sqlite_file() {
    matrix(Cell::Race, Dialect::SqliteFile, None).await;
}

/// The same law on PostgreSQL.
#[tokio::test]
async fn a_race_loser_in_a_cell_is_admitted_and_settles_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    matrix(Cell::Race, Dialect::Postgres, Some(url)).await;
}

/// A cell cut between its end and its consuming commit ends once
/// (FIG-5999), on SQLite in memory: a cell whose calls were all awaited,
/// whose end is held for that commit, and a race, whose loser is closed and
/// whose end is recorded first.
#[tokio::test]
async fn a_cell_cut_between_its_end_and_its_consuming_commit_ends_once_on_sqlite_memory() {
    ends_once(Cell::Identity, Dialect::SqliteMemory, None).await;
    ends_once(Cell::Race, Dialect::SqliteMemory, None).await;
}

/// A cancel accepted before a cell's first save admits nothing, on SQLite
/// in memory (FIG-6000).
#[tokio::test]
async fn a_cancel_accepted_before_a_cells_first_save_admits_nothing_on_sqlite_memory() {
    cancelled_before_save(Dialect::SqliteMemory).await;
}

/// A response held past its first slice is recorded, on SQLite in memory
/// (FIG-6000).
#[tokio::test]
async fn a_response_held_past_its_first_slice_is_recorded_on_sqlite_memory() {
    held_past_its_slice(Dialect::SqliteMemory).await;
}

/// `processes.await` in a cell parks, the session releases and the cell
/// resumes when the process ends, on SQLite in memory.
#[tokio::test]
async fn processes_await_in_a_cell_parks_and_resumes_on_sqlite_memory() {
    await_process(Dialect::SqliteMemory, None).await;
}

/// The same law on a SQLite file.
#[tokio::test]
async fn processes_await_in_a_cell_parks_and_resumes_on_sqlite_file() {
    await_process(Dialect::SqliteFile, None).await;
}

/// The same law on PostgreSQL.
#[tokio::test]
async fn processes_await_in_a_cell_parks_and_resumes_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    await_process(Dialect::Postgres, Some(url)).await;
}

/// A restored cell keeps the deferred grants it resolved, on SQLite in
/// memory (DEFER1).
#[tokio::test]
async fn a_restored_cell_keeps_its_deferred_grants_and_resolves_nothing_twice_on_sqlite_memory() {
    deferred_grant_across_a_crash(Dialect::SqliteMemory, None).await;
}

/// The same law on a SQLite file.
#[tokio::test]
async fn a_restored_cell_keeps_its_deferred_grants_and_resolves_nothing_twice_on_sqlite_file() {
    deferred_grant_across_a_crash(Dialect::SqliteFile, None).await;
}

/// The same law on PostgreSQL.
#[tokio::test]
async fn a_restored_cell_keeps_its_deferred_grants_and_resolves_nothing_twice_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    deferred_grant_across_a_crash(Dialect::Postgres, Some(url)).await;
}

/// A parked cell's committed match is required, on SQLite in memory.
#[tokio::test]
async fn a_parked_cells_committed_match_is_required_on_sqlite_memory() {
    a_parked_cells_committed_match_is_required(Dialect::SqliteMemory).await;
}

/// A parked cell's committed match is required, on SQLite on disk.
#[tokio::test]
async fn a_parked_cells_committed_match_is_required_on_sqlite_file() {
    a_parked_cells_committed_match_is_required(Dialect::SqliteFile).await;
}

/// An after-step stop during a cell's `processes.await` lets the process
/// finish and stops the turn at the step's boundary, on SQLite in memory.
#[tokio::test]
async fn an_after_step_stop_during_a_process_await_lets_the_process_finish_on_sqlite_memory() {
    after_step_during_a_process_await(Dialect::SqliteMemory, None).await;
}

#[tokio::test]
async fn explicit_call_cancel_is_durable_and_selective_before_cell_end() {
    call_cancel::run_law().await;
}

/// The TypeScript source partition of the same law retains its complete
/// matrices under the original action watchdog.
#[tokio::test]
async fn explicit_call_cancel_is_durable_and_selective_before_cell_end_from_typescript() {
    call_cancel::run_typescript_law().await;
}
