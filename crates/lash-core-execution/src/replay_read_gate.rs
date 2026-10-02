//! The replay-read gate (FIG-4324, ADR 0105 §1).
//!
//! A path that can run more than once for one durable effect — a Restate
//! handler's replay, a retried `ctx.run` body, a redrive, a duplicate
//! delivery, a facade call retried under its handler's journal — decides
//! what it records or returns from recorded results, never from a fresh read
//! of mutable store state. A fresh read belongs inside the recorded step whose
//! output it becomes, or is non-durable observation.
//!
//! This gate enforces the half of that rule a source reading can prove: on a
//! replay path, every call to a store read method sits inside a recorded
//! step's span, or is pinned below with its class. It reads the workspace's
//! production sources at run time and verifies containment in the delimiter
//! tree instead of trusting a tag:
//!
//! - **the API surface** is the read methods of the process-registry concern
//!   traits and the trigger store, and the live host services a durable path
//!   consults ([`HOST_SERVICES`]). [`STORE_TRAITS`] and
//!   [`HOST_SERVICE_TRAITS`] are proven complete against the trait
//!   declarations, so a new trait method must be classified before this gate
//!   passes;
//! - **a replay path** is a `fn` whose signature names a controller or journal
//!   context ([`REPLAY_SIGNATURE_TYPES`]), a method of a type that owns one
//!   ([`REPLAY_IMPL_TYPES`]), or a uniquely named `fn` a replay path calls
//!   outside its recorded steps;
//! - **a recorded span** is the argument list of a recorded-step call
//!   ([`RECORDED_STEPS`], or a [`STEP_WRAPPERS`] function proven to hand its
//!   future to one): what a replay reads back is that step's output. A
//!   [`STEP_BODIES`] method proven to be called only inside recorded spans
//!   is part of the step whose span calls it.
//!
//! The other half of the rule — a duplicate delivery re-deciding inside a
//! fresh step what its coalesced admission already decided — happens inside a
//! legitimately recorded step, where no source reading can tell it apart.
//! The replay-after-advance laws own that half.

mod scan;

use std::collections::BTreeMap;

/// Store read methods: each answers today's mutable state.
const STORE_READS: &[&str] = &[
    // ProcessQuery
    "require_process_id",
    "get_process",
    "get_process_by_start_key",
    "list_processes",
    "processes_changed_since",
    "list_non_terminal_processes_page",
    "filter_unregistered_process_ids",
    "filter_tombstoned_process_ids",
    "live_reference_summary",
    "count_non_terminal_processes",
    "list_parked_processes",
    "process_park_feed",
    "summarize_parked_processes",
    // ProcessObserverRegistry
    "is_observer",
    "list_observed_by",
    "list_live_observed_by",
    "observers_for_process",
    // ProcessEventLog
    "event_page",
    "event_page_after",
    "count_events_through",
    "recent_events",
    // ProcessLifecycle
    "terminal_publication",
    "get_parent_end_plan",
    "get_parent_end_plan_by_key",
    "list_parent_end_children",
    "list_unrecorded_opener_parents",
    // ProcessWakeOutbox
    "list_wake_deliveries",
    "wake_delivery_report",
    // ProcessRetention
    "prunable_terminal_processes",
    "list_trigger_delivery_pins",
    // ProcessRegistrationProbe
    "process_is_registered",
    // TriggerStore
    "list_subscriptions",
    "list_occurrences",
    "list_deliveries_by_occurrence_id",
    "list_deliveries_by_subscription_id",
    "list_deliveries_by_process_id",
    "list_deliveries",
    "list_delivery_process_ids",
    "list_delivery_retention_candidates",
    "list_session_owner_ids_for_retention",
];

/// Host-facing reads of today's state that a replay path must not decide
/// from: the facade's session and process lookups.
const HOST_READS: &[&str] = &[
    "session_is_live",
    "require_live_session",
    "session_scope",
    "list_originated_by",
];

/// Live host services, as `(trait, method)`: each answers from the host's
/// wiring and policy today, not from the store. A replay path may consult
/// one only inside the recorded step whose outcome its answer becomes. A call
/// is matched by its receiver's declared trait, never by the method's bare
/// name (FIG-4554).
const HOST_SERVICES: &[(&str, &str)] = &[
    // Reinstalls a delivery's captured provider route, or refuses it. A
    // delivery's start asks it inside its recorded admission
    // (`register_process_start`), off every replay path.
    ("TriggerRouteRestorer", "restore"),
];

/// The traits whose methods are the live host services, with the source that
/// declares each.
const HOST_SERVICE_TRAITS: &[(&str, &str)] = &[(TRIGGERS, "TriggerRouteRestorer")];

/// The surface's other methods: writes, idempotent admissions (which return
/// their retained record, the recorded admission a duplicate decides from)
/// and configuration. Listed so the completeness check can tell a new read
/// from a known write.
const STORE_WRITES: &[&str] = &[
    // ProcessRegistrar
    "register_process",
    "register_process_with_observers",
    "register_process_reporting_outcome",
    "bind_effect_host",
    "set_external_ref",
    // ProcessObserverRegistry
    "add_observer",
    "remove_observer",
    "transfer_observers",
    "retarget_subscription",
    "delete_session_process_state",
    // ProcessEventLog
    "append_event",
    "append_event_with_authority",
    "append_events",
    // ProcessLifecycle
    "complete_process",
    "complete_process_with_prelude",
    "record_parent_end",
    "settle_terminal_publication",
    "settle_parent_end_plan",
    "record_first_started_with_authority",
    "request_process_cancel",
    "request_process_cancel_reporting_realization",
    "record_caller_departure",
    "set_process_wait_with_authority",
    "clear_process_wait_with_authority",
    "park_process_with_authority",
    "begin_parked_rerun_with_authority",
    // ProcessToolIntents
    "admit_tool_intent_submission",
    "complete_tool_intent_submission",
    // ProcessWakeOutbox
    "wake_delivery_config",
    "claim_pending_wake_deliveries",
    "mark_wake_enqueued",
    "discard_wake_delivery",
    "redrive_wake_delivery",
    "defer_wake_delivery",
    // ProcessRetention
    "compact_process_tombstones",
    "compact_process_park_feed",
    "release_process_events",
    "prune_terminal_processes",
    "release_consumer_hold",
    "release_trigger_delivery_pin",
    "abandon_consumer_hold",
    // ProcessClockRebind
    "with_runtime_clock",
    // TriggerStore
    "execute_command",
    "delete_session_subscriptions",
    "ingest_occurrence",
    "bind_delivery_process",
    "reconcile_trigger_retention",
    "delete_delivery_retention_candidates",
    "reclaim_trigger_occurrences",
    "forget_trigger_tombstones",
    "prune_non_fired_occurrences",
];

/// The traits whose methods make up the surface, with the source that
/// declares each (relative to the workspace root).
const STORE_TRAITS: &[(&str, &str)] = &[
    (REGISTRY_CONCERNS, "ProcessQuery"),
    (REGISTRY_CONCERNS, "ProcessRegistrar"),
    (REGISTRY_CONCERNS, "ProcessObserverRegistry"),
    (REGISTRY_CONCERNS, "ProcessEventLog"),
    (REGISTRY_CONCERNS, "ProcessLifecycle"),
    (REGISTRY_CONCERNS, "ProcessToolIntents"),
    (REGISTRY_CONCERNS, "ProcessWakeOutbox"),
    (REGISTRY_CONCERNS, "ProcessRetention"),
    (REGISTRY_CONCERNS, "ProcessClockRebind"),
    (REGISTRY_CONCERNS, "ProcessRegistrationProbe"),
    (TRIGGERS, "TriggerStore"),
];

const TRIGGERS: &str = "crates/lash-core-execution/src/triggers.rs";

const REGISTRY_CONCERNS: &str =
    "crates/lash-core-execution/src/runtime/process/registry_concerns.rs";

/// Calls whose argument list is a recorded step's body: the Restate
/// controller context's journaled runs and the process-command step helper.
/// A Restate `ctx.run(..)` counts too.
const RECORDED_STEPS: &[&str] = &[
    "run_json_send",
    "run_json_or_retry_send",
    "recorded_process_step",
];

/// A `fn` whose signature names one of these runs on a replay path: it holds
/// the scoped controller whose journal a replay reads, or a Restate journal
/// context. A Restate handler's `Context` counts in `lash-restate`.
const REPLAY_SIGNATURE_TYPES: &[&str] = &[
    "ScopedEffectController",
    "RestateRuntimeEffectController",
    "RestateControllerContext",
    "WorkflowContext",
    "SharedWorkflowContext",
    "ObjectContext",
    "SharedObjectContext",
    "ProcessOpScope",
];

/// Every method of these types runs on a replay path although they hold no
/// controller: the tool-intent ingress is redelivered under one durable
/// identity. A type that holds a controller (a struct field or a `type` alias
/// naming one) is found from its definition.
const REPLAY_IMPL_TYPES: &[&str] = &["ToolIntentIngress"];

/// The production sources the gate reads, relative to the workspace root.
const SCANNED_ROOTS: &[&str] = &[
    "crates/lash/src",
    "crates/lash-core/src",
    "crates/lash-core-execution/src",
    "crates/lash-core-worker/src",
    "crates/lash-lashlang-runtime/src",
    "crates/lash-restate/src",
    "examples",
    "runbooks",
];

/// Why a pinned fresh read on a replay path is not a violation.
#[derive(Debug, Clone, Copy)]
enum PinClass {
    /// A read that decides nothing recorded or returned: a host listing, a
    /// snapshot, a cursor.
    Observation(&'static str),
    /// A live revalidation that can only stop stale work before its next
    /// effect, never choose different work (ADR 0105 §1).
    StopOnly(&'static str),
    /// Outside the rule, for the stated reason.
    Exempt(&'static str),
    /// A known violation another ticket owns; its fix deletes the pin.
    Ticket(&'static str),
}

/// One pinned hit: the file, the hit's normalized line text, how many times
/// it occurs, and why it stands.
struct Pin {
    file: &'static str,
    text: &'static str,
    count: usize,
    class: PinClass,
}

const PINS: &[Pin] = &[
    // The generation gate at an invocation's entry refuses a session whose
    // marker this build cannot admit, before any journal read; it never
    // chooses different work.
    Pin {
        file: "crates/lash-core-execution/src/runtime/vocabulary.rs",
        text: "if !session_is_live(store, session_id).await? {",
        count: 2,
        class: PinClass::StopOnly(
            "the session-generation gate refuses before any journal read, and the refused \
             group child's park is a store-side fact of a child that recorded nothing",
        ),
    },
    Pin {
        file: "crates/lash-core-execution/src/tool_provider/process_events.rs",
        text: "if let Ok(true) = crate::session_is_live(factory.as_ref(), &target_session_id).await {",
        count: 1,
        class: PinClass::Observation("gates only a trace of the wake's target session"),
    },
    Pin {
        file: "crates/lash-core-worker/src/runtime/process_worker/mod.rs",
        text: ".get_process(&process_id)",
        count: 1,
        class: PinClass::Exempt(
            "reads the start record the substrate's journaled segment admission wrote \
             (FIG-3588); a running process is never pruned, and its first start is \
             immutable once written",
        ),
    },
    Pin {
        file: "crates/lash-core/src/runtime/session_manager/process_runners/control.rs",
        text: ".get_process(&owner)",
        count: 1,
        class: PinClass::Exempt(
            "the lineage of the live process running this code: immutable, and the \
             process cannot be pruned while it runs",
        ),
    },
    Pin {
        file: "crates/lash-core/src/runtime/session_manager/process_runners/control.rs",
        text: "let enclosing = registry.get_process(&process_id).await?.ok_or_else(|| {",
        count: 1,
        class: PinClass::Exempt(
            "the lineage of the live enclosing process: immutable, and the process \
             cannot be pruned while it runs",
        ),
    },
    Pin {
        file: "crates/lash/src/core/tool_child_context.rs",
        text: "let enclosing = registry.get_process(process_id).await?.ok_or_else(|| {",
        count: 1,
        class: PinClass::Exempt(
            "the lineage of the live enclosing process: immutable, and the process \
             cannot be pruned while it runs",
        ),
    },
    Pin {
        file: "crates/lash-restate/src/process/workflow.rs",
        text: ".get_process(&process_id)",
        count: 1,
        class: PinClass::Exempt(
            "a parked process's rerun marks its park store-side and issues no journal \
             command, so it cannot move the replay",
        ),
    },
    Pin {
        file: "crates/lash-restate/src/process/workflow.rs",
        text: "let parked = match self.registry.get_process(process_id).await {",
        count: 1,
        class: PinClass::Exempt(
            "a diverged segment's park is an idempotent store-side fact written outside \
             the journal it refused",
        ),
    },
    Pin {
        file: "crates/lash-restate/src/process/workflow/lanes.rs",
        text: "let Some(record) = registry.get_process(process_id).await? else {",
        count: 1,
        class: PinClass::Exempt(
            "a generation park is an idempotent store-side fact written outside any \
             segment's journal, fenced by the recorded execution authority",
        ),
    },
];

/// A function that hands one parameter, a future, to a recorded step and
/// touches it nowhere else: a call to it is a recorded step whose argument
/// list is that step's body. The gate proves the shape before it trusts the
/// name: the function is the only one of its name in the scanned tree, it is
/// defined in `file`, and `parameter` occurs only inside the spans of
/// [`RECORDED_STEPS`].
struct StepWrapper {
    file: &'static str,
    function: &'static str,
    parameter: &'static str,
}

const STEP_WRAPPERS: &[StepWrapper] = &[
    // The load behavior workload journals each readback, with its errors,
    // through one `run_json_or_retry_send` (FIG-4484).
    StepWrapper {
        file: LOAD_BEHAVIORS,
        function: "journal_read",
        parameter: "future",
    },
];

/// A method of a module-private trait whose every call sits inside a recorded
/// step's span: its body runs inside that step, so its reads are recorded.
/// The gate proves the trait is private in `declaring_file` and declares
/// `method`. It checks every call named `method` in the declaring directory's
/// production sources, recursively. This includes path-directed sibling
/// modules as well as children in subdirectories, without a file list.
/// A call moved outside its step fails the gate.
struct StepBody {
    declaring_file: &'static str,
    trait_name: &'static str,
    method: &'static str,
}

impl StepBody {
    fn covers(&self, file: &str) -> bool {
        let directory = std::path::Path::new(self.declaring_file)
            .parent()
            .expect("a step body's declaring file has a parent directory");
        !is_test_path(file) && std::path::Path::new(file).starts_with(directory)
    }
}

const STEP_BODIES: &[StepBody] = &[
    // The workload delete's owned-process listing runs inside its recorded
    // `load.model-children.list` step, whose stored IDs execute cancellation
    // (FIG-4348).
    StepBody {
        declaring_file: LOAD_WORKER,
        trait_name: "WorkloadProcessCleanup",
        method: "owned",
    },
];

const TRIGGER_ROUTER: &str = "crates/lash-core-execution/src/triggers/router.rs";
const RESTATE_PROCESS_COMMAND: &str = "crates/lash-restate/src/controller/process_command.rs";
const LOAD_WORKER: &str = "runbooks/restate-postgres-workers/src/load/worker.rs";
const LOAD_BEHAVIORS: &str = "runbooks/restate-postgres-workers/src/load/behaviors.rs";

/// Every failure of the gate over `files`: the step wrappers' and step
/// bodies' proofs, then [`check`] over the hits no proven step body owns.
fn check_tree(
    files: &[(String, String)],
    pins: &[Pin],
    wrappers: &[StepWrapper],
    bodies: &[StepBody],
) -> Vec<String> {
    let survey = scan::survey(files, &surface(wrappers)).expect("the scanned sources tokenize");
    let mut failures = Vec::new();
    for wrapper in wrappers {
        let definitions: Vec<_> = survey
            .definitions
            .iter()
            .filter(|definition| definition.name == wrapper.function)
            .collect();
        let [definition] = definitions.as_slice() else {
            failures.push(format!(
                "step wrapper `{}` must be the only function of its name; found {} definition(s)",
                wrapper.function,
                definitions.len()
            ));
            continue;
        };
        if definition.file != wrapper.file {
            failures.push(format!(
                "step wrapper `{}` is defined in {}, not {}",
                wrapper.function, definition.file, wrapper.file
            ));
        }
        let (inside, outside) =
            scan::ident_uses(&definition.body, wrapper.parameter, RECORDED_STEPS);
        if inside == 0 || outside > 0 {
            failures.push(format!(
                "step wrapper `{}` must hand `{}` to a recorded step and use it nowhere else \
                 ({inside} use(s) inside a recorded step, {outside} outside)",
                wrapper.function, wrapper.parameter
            ));
        }
    }
    for body in bodies {
        let declaring = files
            .iter()
            .find(|(path, _)| path == body.declaring_file)
            .map(|(_, source)| source.as_str())
            .unwrap_or_default();
        if !scan::declares_private_trait(declaring, body.trait_name).unwrap_or(false) {
            failures.push(format!(
                "step body `{}`: {} declares no private trait `{}`",
                body.method, body.declaring_file, body.trait_name
            ));
        }
        if !scan::trait_methods(declaring, body.trait_name)
            .is_ok_and(|methods| methods.iter().any(|method| method == body.method))
        {
            failures.push(format!(
                "step body `{}`: trait `{}` declares no such method",
                body.method, body.trait_name
            ));
        }
        let calls: Vec<_> = survey
            .calls
            .iter()
            .filter(|call| call.name == body.method && body.covers(&call.file))
            .collect();
        if calls.is_empty() {
            failures.push(format!(
                "stale step body `{}`: nothing below {} calls it",
                body.method, body.declaring_file
            ));
        }
        for call in calls.iter().filter(|call| !call.recorded) {
            failures.push(format!(
                "{}:{}: step body `{}` is called outside every recorded step (in `{}`), so its \
                 reads run before the record",
                call.file, call.line, body.method, call.function
            ));
        }
    }
    let unowned: Vec<_> = survey
        .hits
        .into_iter()
        .filter(|hit| {
            !bodies
                .iter()
                .any(|body| hit.function == body.method && body.covers(&hit.file))
        })
        .collect();
    failures.extend(check(&unowned, pins));
    failures
}

/// Every pin failure and every unpinned hit.
fn check(hits: &[scan::Hit], pins: &[Pin]) -> Vec<String> {
    let mut seen: BTreeMap<(&str, &str), Vec<&scan::Hit>> = BTreeMap::new();
    for hit in hits {
        seen.entry((hit.file.as_str(), hit.text.as_str()))
            .or_default()
            .push(hit);
    }
    let mut failures = Vec::new();
    let mut pinned: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for pin in pins {
        if let PinClass::Observation(reason)
        | PinClass::StopOnly(reason)
        | PinClass::Exempt(reason)
        | PinClass::Ticket(reason) = pin.class
            && reason.trim().is_empty()
        {
            failures.push(format!(
                "pin {}  |  {} states no reason",
                pin.file, pin.text
            ));
        }
        *pinned.entry((pin.file, pin.text)).or_default() += pin.count;
    }
    for (key, sites) in &seen {
        let allowed = pinned.get(key).copied().unwrap_or(0);
        if sites.len() > allowed {
            for hit in sites {
                let reads = if STORE_READS.contains(&hit.method.as_str())
                    || HOST_READS.contains(&hit.method.as_str())
                {
                    "reads mutable store state"
                } else {
                    "consults a live host service"
                };
                failures.push(format!(
                    "{}:{}: `{}` {reads} on a replay path outside every recorded step (in \
                     `{}`, on a replay path because {}): {}",
                    hit.file, hit.line, hit.method, hit.function, hit.path, hit.text
                ));
            }
        }
    }
    for ((file, text), count) in &pinned {
        let actual = seen.get(&(*file, *text)).map_or(0, Vec::len);
        if actual < *count {
            failures.push(format!(
                "stale pin {file}  |  {text}  |  {count} (occurs {actual} time(s)): remove or decrement it"
            ));
        }
    }
    failures
}

/// The surface, with each of `wrappers` a recorded step beside
/// [`RECORDED_STEPS`].
fn surface(wrappers: &[StepWrapper]) -> scan::Surface<'static> {
    let steps: Vec<&'static str> = RECORDED_STEPS
        .iter()
        .copied()
        .chain(wrappers.iter().map(|wrapper| wrapper.function))
        .collect();
    scan::Surface {
        reads: READS.get_or_init(|| STORE_READS.iter().chain(HOST_READS).copied().collect()),
        services: HOST_SERVICES,
        steps: Box::leak(steps.into_boxed_slice()),
        replay_signature_types: REPLAY_SIGNATURE_TYPES,
        replay_impl_types: REPLAY_IMPL_TYPES,
    }
}

static READS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Whether `relative` is test code by its path: a `tests` or `testing`
/// directory or module file, or a `*_tests.rs` file.
fn is_test_path(relative: &str) -> bool {
    relative.split('/').any(|part| {
        matches!(
            part,
            "tests" | "testing" | "tests.rs" | "testing.rs" | "test_support.rs"
        ) || part.ends_with("_tests.rs")
            || part.ends_with("_testing.rs")
    })
}

#[expect(
    clippy::disallowed_methods,
    reason = "the gate reads the workspace's own sources, shipped as the test's runfiles"
)]
fn read_sources() -> Vec<(String, String)> {
    let root = workspace_root();
    let mut files = Vec::new();
    for scanned in SCANNED_ROOTS {
        let mut pending = vec![root.join(scanned)];
        while let Some(dir) = pending.pop() {
            let entries = std::fs::read_dir(&dir)
                .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()));
            for entry in entries {
                let path = entry.expect("read a source directory entry").path();
                if path.is_dir() {
                    if !path
                        .file_name()
                        .is_some_and(|name| name == "target" || name == "node_modules")
                    {
                        pending.push(path);
                    }
                    continue;
                }
                if path.extension().is_none_or(|extension| extension != "rs") {
                    continue;
                }
                let relative = path
                    .strip_prefix(&root)
                    .expect("a scanned source lies under the workspace root")
                    .to_string_lossy()
                    .replace('\\', "/");
                if is_test_path(&relative) {
                    continue;
                }
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {relative}: {error}"));
                files.push((relative, source));
            }
        }
    }
    files.sort();
    assert!(
        files
            .iter()
            .any(|(path, _)| path == "crates/lash/src/process_admin.rs"),
        "the gate reads the facade's process admin"
    );
    files
}

#[expect(
    clippy::disallowed_methods,
    reason = "the gate reads the store trait declarations shipped as the test's runfiles"
)]
fn read_workspace_file(relative: &str) -> String {
    std::fs::read_to_string(workspace_root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

#[test]
fn replay_paths_read_store_state_only_inside_recorded_steps() {
    let failures = check_tree(&read_sources(), PINS, STEP_WRAPPERS, STEP_BODIES);
    assert!(
        failures.is_empty(),
        "replay-read gate (ADR 0105 §1): a decision on a replay path must come from a \
         recorded result; move the read into the recorded step that journals it, or pin it \
         with its class\n{}",
        failures.join("\n")
    );
}

#[test]
fn the_store_surface_classifies_every_trait_method() {
    let mut unclassified = Vec::new();
    for (file, trait_name) in STORE_TRAITS {
        let methods = scan::trait_methods(&read_workspace_file(file), trait_name)
            .unwrap_or_else(|error| panic!("{file}: {error}"));
        assert!(!methods.is_empty(), "{trait_name} declares methods");
        for method in methods {
            if !STORE_READS.contains(&method.as_str()) && !STORE_WRITES.contains(&method.as_str()) {
                unclassified.push(format!("{trait_name}::{method}"));
            }
        }
    }
    assert!(
        unclassified.is_empty(),
        "classify each new store method as a read (STORE_READS) or a write (STORE_WRITES): {unclassified:?}"
    );
    for read in STORE_READS {
        assert!(!STORE_WRITES.contains(read), "`{read}` is classified twice");
    }
}

#[test]
fn the_host_services_classify_every_trait_method() {
    let mut declared = Vec::new();
    for (file, trait_name) in HOST_SERVICE_TRAITS {
        let methods = scan::trait_methods(&read_workspace_file(file), trait_name)
            .unwrap_or_else(|error| panic!("{file}: {error}"));
        assert!(!methods.is_empty(), "{trait_name} declares methods");
        declared.extend(methods.into_iter().map(|method| (*trait_name, method)));
    }
    let listed: Vec<_> = HOST_SERVICES
        .iter()
        .map(|(trait_name, method)| (*trait_name, method.to_string()))
        .collect();
    assert_eq!(
        declared, listed,
        "HOST_SERVICES lists exactly the methods its traits declare"
    );
    for (_, service) in HOST_SERVICES {
        assert!(
            !STORE_READS.contains(service) && !STORE_WRITES.contains(service),
            "`{service}` is classified twice"
        );
    }
}

/// The gate's own red side: planted fresh reads fail it, and the shapes the
/// rule allows pass.
mod self_test {
    use super::*;

    fn hits(source: &str) -> Vec<scan::Hit> {
        scan::analyze(
            &[("crates/lash/src/planted.rs".to_string(), source.to_string())],
            &surface(&[]),
        )
        .expect("the planted source tokenizes")
    }

    #[test]
    fn a_planted_read_before_the_journaled_command_fails() {
        let found = hits(
            r#"
            impl Processes {
                pub async fn cancel(&self, id: &ProcessId, scoped: ScopedEffectController<'_>) {
                    let id = self.registry.require_process_id(id).await?;
                    context.run_json_send("cancel", None, async move { admit(id) }).await
                }
            }
            "#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].method, "require_process_id");
        assert_eq!(
            found[0].text,
            "let id = self.registry.require_process_id(id).await?;"
        );
        assert!(
            !check(&found, &[]).is_empty(),
            "an unpinned hit fails the gate"
        );
    }

    #[test]
    fn a_read_inside_the_recorded_step_passes() {
        let found = hits(
            r#"
            async fn guard<'ctx, C>(context: &C, registry: Arc<dyn ProcessRegistry>)
            where C: RestateControllerContext<'ctx>
            {
                let Json(guarded) = context
                    .run_json_or_retry_send::<Result<(), PluginError>, _>(
                        name,
                        async move { registry.get_process(&id).await },
                    )
                    .await?;
                recorded_process_step(context, invocation, "x", async move {
                    registry.list_observed_by(&session, &filter).await
                })
                .await
            }
            "#,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_helper_called_outside_a_step_carries_the_replay_path() {
        let found = hits(
            r#"
            async fn validate(registry: &dyn ProcessRegistry, id: &ProcessId) -> bool {
                registry.get_process(id).await.is_ok()
            }
            async fn signal(scoped: ScopedEffectController<'_>) {
                if validate(&registry, &id).await { issue(scoped).await }
            }
            "#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].function, "validate");
        assert_eq!(found[0].path, "called from `signal`");
    }

    #[test]
    fn a_helper_called_only_inside_steps_passes() {
        let found = hits(
            r#"
            async fn existence_guard(registry: &dyn ProcessRegistry, id: &ProcessId) -> bool {
                registry.require_process_id(id).await.is_ok()
            }
            async fn attach(ctx: WorkflowContext<'_>) {
                ctx.run(|| async { existence_guard(&registry, &id).await }).await
            }
            "#,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn reads_off_every_replay_path_and_in_test_code_pass() {
        let found = hits(
            r#"
            pub async fn list(&self, filter: &ProcessListFilter) -> Vec<ProcessRecord> {
                self.registry.list_processes(filter).await
            }
            #[cfg(test)]
            mod tests {
                async fn law(scoped: ScopedEffectController<'_>) {
                    registry.get_process(&id).await;
                }
            }
            #[tokio::test]
            async fn law(scoped: ScopedEffectController<'_>) {
                registry.get_process(&id).await;
            }
            "#,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_host_service_call_outside_a_recorded_step_fails() {
        let found = hits(
            r#"
            pub struct Router {
                store: Arc<dyn TriggerStore>,
                pub(crate) route_restorer: Option<Arc<dyn TriggerRouteRestorer>>,
            }
            async fn prepare(&self, capture: &TriggerSourceCapture) -> Result<(), Refusal> {
                self.route_restorer.as_ref()?.restore(capture).await
            }
            async fn start(&self, scoped: &ScopedEffectController<'_>) {
                self.prepare(&capture).await?;
                TriggerRouteRestorer::restore(unnamed.as_ref(), &capture).await?;
                scoped.execute_effect(envelope, executor).await
            }
            async fn bound(&self, scoped: &ScopedEffectController<'_>) {
                if let Some(restorer) = self.route_restorer.as_ref() {
                    restorer.restore(&capture).await?;
                }
            }
            async fn handed<R>(scoped: &ScopedEffectController<'_>, host: &R, other: Arc<dyn TriggerRouteRestorer>)
            where
                R: TriggerRouteRestorer,
            {
                let cloned = Arc::clone(&other);
                host.restore(&capture).await?;
                cloned.restore(&capture).await
            }
            "#,
        );
        assert_eq!(found.len(), 5, "{found:?}");
        assert!(found.iter().all(|hit| hit.method == "restore"));
        assert_eq!(found[0].function, "prepare");
        assert_eq!(found[0].path, "called from `start`");
        assert_eq!(
            found
                .iter()
                .map(|hit| hit.function.as_str())
                .collect::<Vec<_>>(),
            ["prepare", "start", "bound", "handed", "handed"]
        );
        assert!(
            check(&found, &[])
                .iter()
                .all(|failure| failure.contains("consults a live host service")),
            "an unpinned service call fails the gate"
        );
    }

    #[test]
    fn host_service_closure_parameters_are_followed() {
        let found = hits(
            r#"
            struct Router { service: Option<Arc<dyn TriggerRouteRestorer>> }
            async fn start(&self, scoped: ScopedEffectController<'_>) {
                let typed = |host: Arc<dyn TriggerRouteRestorer>| async move {
                    host.restore(&capture).await
                };
                self.service.as_ref().map(|mapped| async move {
                    mapped.restore(&capture).await
                });
                let unrelated = |other: ReplayOrdinals| other.restore(&state);
            }
        "#,
        );
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            check(&found, &[])
                .iter()
                .all(|failure| failure.contains("consults a live host service"))
        );
    }

    #[test]
    fn host_service_match_arm_bindings_are_followed() {
        let found = hits(
            r#"
            struct Router { service: Option<Arc<dyn TriggerRouteRestorer>> }
            async fn start(&self, scoped: ScopedEffectController<'_>) {
                match self.service.as_ref() {
                    Some(matched) => matched.restore(&capture).await,
                    None => Ok(()),
                }
                let alias = self.service.clone();
                match alias {
                    Some(ref borrowed) => { borrowed.restore(&capture).await; }
                    None => (),
                }
                match unrelated {
                    Some(other) => other.restore(&state),
                    None => (),
                }
            }
        "#,
        );
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            check(&found, &[])
                .iter()
                .all(|failure| failure.contains("consults a live host service"))
        );
    }

    #[test]
    fn a_host_service_call_inside_a_step_or_of_another_type_passes() {
        let found = hits(
            r#"
            async fn start(context: &impl RestateControllerContext<'_>) {
                let ordinals = ReplayOrdinals::restore(state.as_ref());
                context
                    .run_json_or_retry_send(name, async move { restorer.restore(&capture).await })
                    .await
            }
            "#,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    /// A method named as a live service is one only on a receiver declared
    /// with the service's trait: any other `.restore(` on a replay path is
    /// no hit (FIG-4554).
    #[test]
    fn a_same_named_method_on_another_receiver_is_no_service_call() {
        let found = hits(
            r#"
            struct Router {
                route_restorer: Option<Arc<dyn TriggerRouteRestorer>>,
                ordinals: ReplayOrdinals,
            }
            async fn start(&self, scoped: &ScopedEffectController<'_>, snapshot: &Snapshot) {
                self.ordinals.restore(snapshot);
                let cursor = Cursor::open(&self.store);
                cursor.restore(snapshot).await?;
                snapshot.restore(&self.ordinals)?;
                scoped.execute_effect(envelope, executor).await
            }
            "#,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_method_of_a_controller_owning_type_is_a_replay_path() {
        let found = hits(
            r#"
            struct ProcessCommandRunner<'scope> {
                registry: Arc<dyn ProcessRegistry>,
                scoped_effect_controller: ScopedEffectController<'scope>,
            }
            impl<'scope> ProcessCommandRunner<'scope> {
                async fn cancel_named(&self, id: &ProcessId) {
                    let command = match self.registry.require_process_id(id).await { _ => () };
                }
            }
            "#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].path,
            "it is a method of `ProcessCommandRunner`, a replay-path type"
        );
    }

    #[test]
    fn a_type_holding_an_aliased_controller_is_a_replay_path() {
        let found = hits(
            r#"
            mod controllers {
                type RecordedController<'ctx> = RestateRuntimeEffectController<'ctx, WorkflowContext<'ctx>>;
                pub type Controller<'ctx> = RecordedController<'ctx>;
            }
            struct SessionProcessCleanup<'a, 'ctx> {
                processes: Processes,
                controller: &'a controllers::Controller<'ctx>,
            }
            impl WorkloadProcessCleanup for SessionProcessCleanup<'_, '_> {
                async fn owned(&self, session: &str) -> Vec<ProcessId> {
                    self.processes.list_originated_by(&scope, &filter).await
                }
            }
            "#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].method, "list_originated_by");
    }

    #[test]
    fn a_controller_associated_type_does_not_seed_an_unrelated_target() {
        for declaration in [
            "impl std::ops::Deref for Driver { type Target = RestateRuntimeEffectController; }",
            "trait Driver { type Target = RestateRuntimeEffectController; }",
        ] {
            let found = hits(&format!(
                r#"
                {declaration}
                struct Target {{ revision: u64 }}
                async fn fork_at(target: Target) {{
                    registry.get_process(&id).await
                }}
                "#,
            ));
            assert!(found.is_empty(), "{declaration}: {found:?}");
        }
    }

    #[test]
    fn a_method_on_another_receiver_carries_no_replay_path_by_name() {
        let found = hits(
            r#"
            async fn recv(&mut self) -> Event {
                self.registry.get_process(&self.id).await
            }
            async fn shift(scoped: ScopedEffectController<'_>) {
                let event = channel.recv().await;
            }
            "#,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_continued_string_keeps_hit_lines_exact() {
        let found = hits(
            "async fn signal(scoped: ScopedEffectController<'_>) {\n\
             let _ = \"one \\\n two\";\n\
             registry.get_process(&id).await;\n\
             }\n",
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].line, 4);
        assert_eq!(found[0].text, "registry.get_process(&id).await;");
    }

    #[test]
    fn literals_comments_and_lifetimes_do_not_confuse_the_tree() {
        let found = hits(
            r##"
            // registry.get_process(&id) in a comment is no call
            async fn signal<'a>(scoped: ScopedEffectController<'a>) {
                let _ = "registry.get_process(&id) (";
                let _ = r#"}{ registry.get_process"#;
                let _ = '{';
                /* nested /* } */ comment */
                context.run_json_send("s", None, async move { registry.get_process(&id).await }).await;
            }
            "##,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn pins_match_by_text_and_count_and_go_stale() {
        let found = hits(
            r#"
            async fn cancel_all(scoped: ScopedEffectController<'_>) {
                let running = self.registry.list_processes(&filter).await?;
            }
            "#,
        );
        let pin = |count| Pin {
            file: "crates/lash/src/planted.rs",
            text: "let running = self.registry.list_processes(&filter).await?;",
            count,
            class: PinClass::Ticket("FIG-0000"),
        };
        assert!(check(&found, &[pin(1)]).is_empty(), "a matching pin passes");
        assert!(
            check(&found, &[pin(0)])
                .iter()
                .any(|f| f.contains("outside every recorded step"))
        );
        assert!(
            check(&found, &[pin(2)])
                .iter()
                .any(|f| f.starts_with("stale pin"))
        );
        assert!(
            check(&[], &[pin(1)])
                .iter()
                .any(|f| f.starts_with("stale pin"))
        );
    }

    /// A registry read planted ahead of the real facade's journaled cancel
    /// command fails the gate over the real tree.
    #[test]
    fn the_real_tree_fails_with_a_facade_read_planted_before_its_command() {
        let mut files = read_sources();
        let (_, facade) = files
            .iter_mut()
            .find(|(path, _)| path == "crates/lash/src/process_admin.rs")
            .expect("the facade's process admin is scanned");
        let anchor = "let command = lash_core::ProcessCommand::Cancel {";
        assert!(
            facade.contains(anchor),
            "the facade cancel builds its command"
        );
        *facade = facade.replacen(
            anchor,
            &format!(
                "let _planted = self.core.process_registry().require_process_id(process_id).await?;\n{anchor}"
            ),
            1,
        );
        let failures = check_tree(&files, PINS, STEP_WRAPPERS, STEP_BODIES);
        assert!(
            failures.iter().any(|failure| failure.contains("_planted")),
            "the planted read fails the gate: {failures:?}"
        );
    }

    /// The real tree, with `anchor` in `path` preceded by `planted`.
    fn planted_tree(path: &str, anchor: &str, planted: &str) -> Vec<String> {
        let mut files = read_sources();
        let (_, source) = files
            .iter_mut()
            .find(|(file, _)| file == path)
            .unwrap_or_else(|| panic!("{path} is scanned"));
        assert!(source.contains(anchor), "{path} holds `{anchor}`");
        *source = source.replacen(anchor, &format!("{planted}\n{anchor}"), 1);
        check_tree(&files, PINS, STEP_WRAPPERS, STEP_BODIES)
    }

    #[test]
    fn the_real_tree_passes_with_no_ticket_pins() {
        assert!(
            PINS.iter()
                .all(|pin| !matches!(pin.class, PinClass::Ticket(_))),
            "every known violation is fixed"
        );
        assert!(check_tree(&read_sources(), PINS, STEP_WRAPPERS, STEP_BODIES).is_empty());
    }

    /// A promotion lookup moved ahead of its recorded step fails the gate.
    #[test]
    fn the_real_tree_fails_with_an_unrecorded_promotion_lookup() {
        let failures = planted_tree(
            LOAD_BEHAVIORS,
            r#"journal_read(controller, "load.promotion""#,
            "let _planted = core.process_registry().get_process(&lash_core::ProcessId::new()).await;",
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("_planted") && failure.contains("get_process")),
            "{failures:?}"
        );
    }

    /// An owned-process listing moved outside its recorded step fails the
    /// gate, though the listing's body is unchanged.
    #[test]
    fn the_real_tree_fails_with_an_owned_listing_outside_its_step() {
        let failures = planted_tree(
            LOAD_WORKER,
            "let mut cleaned = std::collections::BTreeSet::new();",
            "let _planted = admin.owned(session).await;",
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("step body `owned` is called outside")),
            "{failures:?}"
        );
    }

    /// A route-restorer call planted in `prepare_delivery_start`, ahead of
    /// the delivery start's recorded step, fails the gate (FIG-4537).
    #[test]
    fn the_real_tree_fails_with_an_unrecorded_route_restore_before_the_delivery_start() {
        for planted in [
            "let _planted = self.route_restorer.as_ref().expect(\"wired\").restore(&subscription.source_capture);",
            "if let Some(restorer) = self.route_restorer.as_ref() { let _planted = restorer.restore(&subscription.source_capture); }",
        ] {
            let failures = planted_tree(TRIGGER_ROUTER, "let args =", planted);
            assert!(
                failures.iter().any(|failure| failure.contains("_planted")
                    && failure.contains("`restore`")
                    && failure.contains("in `prepare_delivery_start`")),
                "{planted}: {failures:?}"
            );
        }
    }

    /// An unrelated `.restore(` planted at the same place passes: its
    /// receiver is not the route restorer (FIG-4554).
    #[test]
    fn the_real_tree_passes_with_an_unrelated_restore_before_the_delivery_start() {
        let failures = planted_tree(
            TRIGGER_ROUTER,
            "let args =",
            "let _planted = occurrence.payload.restore(&subscription.source_capture);",
        );
        assert!(failures.is_empty(), "{failures:?}");
    }

    /// The route restore moved out of the start's recorded admission, into
    /// the Restate start ahead of its registration step, fails the gate
    /// (FIG-4554).
    #[test]
    fn the_real_tree_fails_with_the_route_restored_ahead_of_the_restate_registration() {
        let failures = planted_tree(
            RESTATE_PROCESS_COMMAND,
            "let stored_registration = registration.clone();",
            "let restorer: Arc<dyn lash_core::TriggerRouteRestorer> = host_restorer(); let _planted = restorer.restore(&capture).await;",
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("_planted") && failure.contains("`restore`")),
            "{failures:?}"
        );
    }

    #[test]
    fn a_new_worker_module_is_covered_without_editing_the_proof() {
        let mut files = read_sources();
        for path in [
            "runbooks/restate-postgres-workers/src/load/new_cleanup.rs",
            "runbooks/restate-postgres-workers/src/load/worker/new_cleanup.rs",
        ] {
            files.push((
                path.to_owned(),
                "async fn cleanup(admin: &impl WorkloadProcessCleanup) { admin.owned(session).await; }"
                    .to_owned(),
            ));
            let failures = check_tree(&files, PINS, STEP_WRAPPERS, STEP_BODIES);
            assert!(
                failures.iter().any(|failure| failure.starts_with(path)
                    && failure.contains("step body `owned` is called outside")),
                "a new worker module's unrecorded listing must fail the proof: {failures:?}"
            );
            files.last_mut().unwrap().1 =
                "async fn cleanup(ctx: WorkflowContext<'_>, admin: &impl WorkloadProcessCleanup) { ctx.run(|| async { admin.owned(session).await }).await; }"
                    .to_owned();
            assert!(check_tree(&files, PINS, STEP_WRAPPERS, STEP_BODIES).is_empty());
        }
    }

    /// A wrapper that awaits its future before the step it names records
    /// nothing, and fails the gate.
    #[test]
    fn a_wrapper_that_runs_its_future_outside_the_step_fails() {
        let planted = r#"
            type Controller<'ctx> = RestateRuntimeEffectController<'ctx, WorkflowContext<'ctx>>;
            async fn journal_read<T, F>(controller: &Controller<'_>, name: &str, future: F) -> T {
                let answer = future.await;
                controller.context().run_json_or_retry_send(name.into(), async move { Ok(answer) }).await
            }
            async fn read(controller: &Controller<'_>) {
                journal_read(controller, "x", async { registry.get_process(&id).await }).await;
            }
        "#;
        let files = [(LOAD_BEHAVIORS.to_string(), planted.to_string())];
        let failures = check_tree(&files, &[], STEP_WRAPPERS, &[]);
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("must hand `future` to a recorded step")),
            "{failures:?}"
        );
        let honest = planted.replace("let answer = future.await;\n", "").replace(
            "async move { Ok(answer) }",
            "async move { Ok(future.await) }",
        );
        let files = [(LOAD_BEHAVIORS.to_string(), honest)];
        assert!(
            check_tree(&files, &[], STEP_WRAPPERS, &[]).is_empty(),
            "a read inside a proven wrapper's span passes"
        );
    }
}
