//! Global invariants every simulated history must keep (FIG-4086).
//!
//! A scenario asserts what it was written to prove. The checkers here assert
//! what every history must keep, whatever the scenario, the seed or the
//! fault schedule, so a contract nobody wrote a scenario for still fails a
//! run that breaks it.
//!
//! A [`History`] is a finished run: its ordered trace [`Record`]s and the
//! final durable rows of every store the run wrote ([`StoreSnapshot`]). Each
//! [`HistoryChecker`] reads a history and returns its [`Violation`]s;
//! [`CHECKERS`] is the one registry [`check`] runs. Adding a checker is one
//! file in this module and one line in [`CHECKERS`].
//!
//! Where each checker's facts come from:
//!
//! | Checker | Facts |
//! |---|---|
//! | [`completion_ownership`] | completion keys the sim's deferring tools registered and the host resolved ([`Fact::CompletionRegistered`], [`Fact::CompletionResolved`]); the committed transcript's tool results |
//! | [`effect_window`] | runs of the sim's tool bodies with the engine's failed-attempt count at each run ([`Fact::ToolExecuted`]); counted executions of the effects the harness runs ([`Fact::EffectRan`]) |
//! | [`obligations_settled`] | every ADR 0109 obligation column family in every store database |
//! | [`artifact_reachability`] | `artifact_refs`, `artifact_referrer_edges`, `artifact_referrer_fences` and `artifact_cleanup_obligations` |
//! | [`tool_call_identity`] | tool-body runs ([`Fact::ToolExecuted`]) and the committed transcript's tool calls |
//! | [`input_settlement`] | `pending_turn_inputs`, `queued_work_batches`, `session_roots` and `session_root_inputs` |
//! | [`start_originator`] | the host's process starts, each with the originator it requested and the one the answered process carries ([`Fact::ProcessStartAnswered`]) |
//!
//! Store rows are read raw, through the SQLite store's test-only
//! `read_rows_for_testing` (`lash-sqlite-store`, `testing` feature), the
//! one observation hook this module added outside lash-sim. Tool facts are
//! recorded by lash-sim's own tools through a [`HistoryRecorder`]; nothing
//! in lash's runtime records them.
//!
//! Where the checkers run: every generated-world seed (search and evidence
//! lanes, as the run-only oracle [`GLOBAL_INVARIANTS_ORACLE`]), every
//! crash-matrix cell and seed once its end state holds, every chaos-soak
//! epoch, and the pending-tool scenario on the Restate server double.
//!
//! A violation prints its seed, its invariant, the trace records and the store
//! rows that show it. The seed names the run's inputs; it does not reproduce
//! the run's interleaving, so the printed history is what a triage reads.
//!
//! A violation a known runtime defect causes is [`quarantine`]d by name: it is
//! still printed with its seed and excerpt, and it no longer fails the run.

mod artifact_reachability;
mod completion_ownership;
mod effect_window;
mod input_settlement;
mod obligations_settled;
pub mod quarantine;
mod snapshot;
mod start_originator;
mod tool_call_identity;

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use lash_core::sync::MutexExt as _;
use serde::Serialize;

pub use snapshot::{
    ArtifactRow, CleanupRow, InputRow, ObligationRow, RootRow, StoreSnapshot, TranscriptCall,
    TranscriptResult, TranscriptSession,
};

/// The oracle a generated run reports its global-invariant verdict under.
pub const GLOBAL_INVARIANTS_ORACLE: &str = "sim.oracle.global-invariants.v1";

/// How many of its session's last records a violation that names none shows.
const EXCERPT_RECORDS: usize = 12;

/// A finished simulated run.
#[derive(Clone, Debug, Default, Serialize)]
pub struct History {
    /// The scenario that produced it: a profile, a crash-matrix cell, a soak
    /// epoch.
    pub scenario: String,
    pub seed: u64,
    /// The run's trace, in order. A record's [`Record::at`] is its index.
    pub records: Vec<Record>,
    /// The final durable rows of every store the run wrote.
    pub stores: Vec<StoreSnapshot>,
    /// Whether the history ends after a recovery pass that followed its last
    /// write (the crash matrix and the soak run one before they are judged).
    /// Without one, work only that pass delivers (ADR 0113 §2.5's artifact
    /// cleanups) is still owed to it, not dropped.
    pub relay_ran: bool,
    /// The store clock's epoch milliseconds when the stores were read.
    pub now_ms: Option<u64>,
}

impl History {
    #[must_use]
    pub fn new(scenario: impl Into<String>, seed: u64) -> Self {
        Self {
            scenario: scenario.into(),
            seed,
            records: Vec::new(),
            stores: Vec::new(),
            relay_ran: false,
            now_ms: None,
        }
    }

    /// Mark the history as ending after the recovery pass ran to quiescence.
    #[must_use]
    pub fn after_relay(mut self) -> Self {
        self.relay_ran = true;
        self
    }

    /// When the last claim this history ends holding lapses: the latest
    /// `due_at_ms` of a `claimed` obligation, which is its claim's expiry
    /// (ADR 0109 §1.1). `None` when no obligation is claimed.
    #[must_use]
    pub fn last_claim_lapse_ms(&self) -> Option<u64> {
        self.stores
            .iter()
            .flat_map(|store| &store.obligations)
            .filter(|row| row.state.as_deref() == Some("claimed"))
            .filter_map(|row| row.due_at_ms)
            .max()
    }

    /// Append `fact` to the trace.
    pub fn push(&mut self, fact: Fact) {
        let at = self.records.len();
        self.records.push(Record { at, fact });
    }

    /// Append every fact `recorder` holds, in the order they were recorded.
    pub fn extend_from(&mut self, recorder: &HistoryRecorder) {
        for fact in recorder.facts() {
            self.push(fact);
        }
    }

    /// Read `stores`' final rows under `label` and add them.
    pub fn capture_store(
        &mut self,
        label: impl Into<String>,
        stores: &lash_sqlite_store::SqliteStoreSet,
    ) -> Result<(), String> {
        self.stores.push(StoreSnapshot::read(label, stores)?);
        Ok(())
    }

    /// [`capture_store`](Self::capture_store), plus each session's committed
    /// transcript read back through the store set's session factory.
    pub async fn capture_store_with_transcripts(
        &mut self,
        label: impl Into<String>,
        stores: &lash_sqlite_store::SqliteStoreSet,
    ) -> Result<(), String> {
        let mut snapshot = StoreSnapshot::read(label, stores)?;
        snapshot.read_transcripts(stores).await?;
        self.stores.push(snapshot);
        Ok(())
    }
}

/// One trace record.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Record {
    pub at: usize,
    pub fact: Fact,
}

/// Today's identity of one tool call: the call id the tool saw.
///
/// EXTENSION POINT (FIG-4080): today this is the provider's call id, a
/// string the provider may repeat. When lash hands every tool its sealed
/// `lash_sansio::ToolCallId` (ADR 0117), this becomes that id — recorded from
/// `AttemptContext::call_id()` in [`ToolObserver::executed`] and read from the
/// transcript part's id — and [`tool_call_identity`] then proves uniqueness
/// deployment-wide rather than per session.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CallIdentity(pub String);

impl std::fmt::Display for CallIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One logical tool call as the tool saw it.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CallRef {
    pub session: String,
    /// The execution scope the call ran under.
    pub scope: String,
    /// Where the call sits, independent of its id: the engine's effect
    /// address of the call's attempt (its replay key), which crash replay
    /// keeps and which differs between two calls.
    pub logical: String,
    pub identity: CallIdentity,
}

/// One trace fact.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "fact", rename_all = "snake_case")]
pub enum Fact {
    /// A scheduler boundary the run delivered.
    Boundary {
        boundary_id: String,
        actor: String,
        kind: String,
        label: String,
        /// The ingress item the boundary admitted or withdrew, when it names
        /// one.
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<String>,
    },
    /// One run of a tool body.
    ToolExecuted {
        call: CallRef,
        /// Lash's attempt number: a reported-failure retry is a new attempt.
        attempt: u32,
        /// Attempts the engine had seen fail (crash or retry) when the body
        /// ran: a body may run again only across one.
        failed_attempts_before: u64,
    },
    /// A deferring call registered its completion key.
    CompletionRegistered { key: String, call: CallRef },
    /// The host resolved `key`; the committed result that carries it reads
    /// as `result_digest`.
    CompletionResolved {
        key: String,
        session: String,
        result_digest: String,
    },
    /// A host process start answered: the process, whether the registrar
    /// created it or found it, the originator the request carried and the
    /// one the answered process carries.
    ProcessStartAnswered {
        process: String,
        disposition: String,
        requested_originator: String,
        answered_originator: String,
    },
    /// An effect the harness ran through a counted executor.
    EffectRan {
        effect: String,
        executions: usize,
        /// Attempts that died after running the effect and before its
        /// outcome was recorded: the at-least-once window.
        unrecorded_attempts: usize,
    },
}

impl Fact {
    /// The session a fact concerns, when it names one.
    #[must_use]
    pub fn session(&self) -> Option<&str> {
        match self {
            Self::Boundary { actor, .. } => Some(actor),
            Self::ToolExecuted { call, .. } | Self::CompletionRegistered { call, .. } => {
                Some(&call.session)
            }
            Self::CompletionResolved { session, .. } => Some(session),
            Self::EffectRan { .. } | Self::ProcessStartAnswered { .. } => None,
        }
    }
}

/// Facts lash-sim's own tools and host record while a run is live.
///
/// Cloning shares the log. Recording takes a std mutex and never awaits, so
/// a tool that records hands no turn to the engine's scheduler.
#[derive(Clone, Debug, Default)]
pub struct HistoryRecorder {
    facts: Arc<Mutex<Vec<Fact>>>,
}

impl HistoryRecorder {
    pub fn record(&self, fact: Fact) {
        self.facts.lock_recover().push(fact);
    }

    #[must_use]
    pub fn facts(&self) -> Vec<Fact> {
        self.facts.lock_recover().clone()
    }
}

/// What a sim tool records about its own runs: a recorder and the server
/// double whose attempts run it.
#[derive(Clone)]
pub struct ToolObserver {
    recorder: HistoryRecorder,
    server: Option<lash_restate_test::RestateTestServer>,
}

impl std::fmt::Debug for ToolObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolObserver")
            .finish_non_exhaustive()
    }
}

impl ToolObserver {
    #[must_use]
    pub fn new(
        recorder: HistoryRecorder,
        server: Option<lash_restate_test::RestateTestServer>,
    ) -> Self {
        Self { recorder, server }
    }

    /// The call `context` runs, as a [`CallRef`].
    #[must_use]
    pub fn call(context: &lash_core::AttemptContext<'_>) -> CallRef {
        CallRef {
            session: context.session_id().to_owned(),
            scope: context.execution_scope_id().to_owned(),
            logical: context.replay_key().unwrap_or_default().to_owned(),
            identity: CallIdentity(context.tool_call_id().unwrap_or_default().to_owned()),
        }
    }

    /// Record one run of a tool body under `context`.
    pub fn executed(&self, context: &lash_core::AttemptContext<'_>) -> CallRef {
        let call = Self::call(context);
        let failed_attempts_before = self.server.as_ref().map_or(0, |server| {
            let stats = server.stats();
            stats.crashes + stats.retries
        });
        self.recorder.record(Fact::ToolExecuted {
            call: call.clone(),
            attempt: context.attempt_number(),
            failed_attempts_before,
        });
        call
    }

    /// Record that `call` registered `key`.
    pub fn registered(&self, call: CallRef, key: &lash_core::AwaitEventKey) {
        self.recorder.record(Fact::CompletionRegistered {
            key: completion_key_label(key),
            call,
        });
    }
}

/// A completion key as a history names it.
#[must_use]
pub fn completion_key_label(key: &lash_core::AwaitEventKey) -> String {
    serde_json::to_string(key).unwrap_or_else(|_| format!("{key:?}"))
}

/// Record that a host start requested under `requested` was answered with
/// `receipt`, whose process the registry holds as `answered`.
pub fn record_start_answered(
    recorder: &HistoryRecorder,
    requested: &lash_core::ProcessOriginator,
    receipt: &lash_core::ProcessStartReceipt,
    answered: &lash_core::ProcessRecord,
) {
    let originator = |originator: &lash_core::ProcessOriginator| {
        serde_json::to_string(originator).unwrap_or_else(|_| format!("{originator:?}"))
    };
    recorder.record(Fact::ProcessStartAnswered {
        process: receipt.process_id.to_string(),
        disposition: match receipt.disposition {
            lash_core::ProcessRegistrationDisposition::Created => "created",
            lash_core::ProcessRegistrationDisposition::Existing => "existing",
        }
        .to_owned(),
        requested_originator: originator(requested),
        answered_originator: originator(&answered.provenance.originator),
    });
}

/// The digest a committed tool result's text is compared by.
#[must_use]
pub fn result_digest(content: &str) -> String {
    crate::trace::value_digest(&serde_json::Value::String(content.to_owned()))
}

/// One broken invariant.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Violation {
    /// The checker's [`HistoryChecker::invariant`].
    pub invariant: &'static str,
    pub detail: String,
    /// The trace records that show it, by [`Record::at`].
    pub records: Vec<usize>,
    /// The store rows that show it, rendered.
    pub rows: Vec<String>,
    /// The session it concerns, for an excerpt with no named records.
    pub session: Option<String>,
}

impl Violation {
    #[must_use]
    pub fn new(invariant: &'static str, detail: impl Into<String>) -> Self {
        Self {
            invariant,
            detail: detail.into(),
            records: Vec::new(),
            rows: Vec::new(),
            session: None,
        }
    }

    #[must_use]
    pub fn records(mut self, records: impl IntoIterator<Item = usize>) -> Self {
        self.records.extend(records);
        self
    }

    #[must_use]
    pub fn row(mut self, row: impl Into<String>) -> Self {
        self.rows.push(row.into());
        self
    }

    #[must_use]
    pub fn session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }
}

/// One global invariant.
pub trait HistoryChecker: Sync {
    /// The invariant's stable name.
    fn invariant(&self) -> &'static str;

    /// Every place `history` breaks it.
    fn check(&self, history: &History) -> Vec<Violation>;

    /// How many facts this history gave it to judge. A pass over nothing is
    /// reported as such, never as a proof.
    fn observed(&self, history: &History) -> usize;
}

/// Every checker [`check`] runs, in report order.
pub static CHECKERS: &[&dyn HistoryChecker] = &[
    &completion_ownership::CompletionOwnership,
    &effect_window::EffectWindow,
    &obligations_settled::ObligationsSettled,
    &artifact_reachability::ArtifactReachability,
    &tool_call_identity::ToolCallIdentity,
    &input_settlement::InputSettlement,
    &start_originator::StartOriginator,
];

/// Every checker's verdict on one history.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub scenario: String,
    pub seed: u64,
    /// Each checker's invariant and how many facts it judged.
    pub observed: Vec<(&'static str, usize)>,
    /// Violations no quarantine entry covers: these fail the run.
    pub violations: Vec<Violation>,
    /// Violations a quarantine entry covers, with the entry's name.
    pub quarantined: Vec<(&'static str, Violation)>,
    /// The rendered excerpt of every violation, quarantined ones included.
    pub rendered: Vec<String>,
}

impl Report {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }

    /// One line per checker: what it judged.
    #[must_use]
    pub fn summary(&self) -> String {
        let counts = self
            .observed
            .iter()
            .map(|(invariant, count)| format!("{invariant}={count}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "{} seed {:#018x}: {} violation(s), {} quarantined; judged {counts}",
            self.scenario,
            self.seed,
            self.violations.len(),
            self.quarantined.len()
        )
    }

    /// The failing violations, each with its seed, invariant and excerpt.
    #[must_use]
    pub fn failure(&self) -> String {
        self.rendered[..self.violations.len()].join("\n")
    }

    /// Print every quarantined violation, so a quarantined defect stays
    /// visible in the run's log.
    pub fn print_quarantined(&self) {
        let offset = self.violations.len();
        for (index, (entry, _)) in self.quarantined.iter().enumerate() {
            println!("quarantined by {entry}:\n{}", self.rendered[offset + index]);
        }
    }

    /// The generated lane's verdict.
    #[must_use]
    pub fn verdict(&self) -> crate::trace::OracleVerdict {
        if self.passed() {
            crate::trace::OracleVerdict::passed(GLOBAL_INVARIANTS_ORACLE, self.summary())
        } else {
            crate::trace::OracleVerdict::failed(
                GLOBAL_INVARIANTS_ORACLE,
                format!("{}\n{}", self.summary(), self.failure()),
            )
        }
    }
}

/// Run every registered checker over `history`.
#[must_use]
pub fn check(history: &History) -> Report {
    check_with(history, CHECKERS)
}

/// Run `checkers` over `history`: [`check`] with a chosen registry.
#[must_use]
pub fn check_with(history: &History, checkers: &[&dyn HistoryChecker]) -> Report {
    let mut violations = Vec::new();
    let mut quarantined = Vec::new();
    let mut observed = Vec::new();
    for checker in checkers {
        observed.push((checker.invariant(), checker.observed(history)));
        for violation in checker.check(history) {
            match quarantine::covering(&history.scenario, &violation) {
                Some(entry) => quarantined.push((entry.name, violation)),
                None => violations.push(violation),
            }
        }
    }
    let rendered = violations
        .iter()
        .chain(quarantined.iter().map(|(_, violation)| violation))
        .map(|violation| render(history, violation))
        .collect();
    Report {
        scenario: history.scenario.clone(),
        seed: history.seed,
        observed,
        violations,
        quarantined,
        rendered,
    }
}

/// A violation with its seed, invariant and trace excerpt: every record it
/// names, or, when it names none, the last records of its session; then the
/// store rows it names. A seed reproduces a run's inputs, not its
/// interleavings, so what this prints is the evidence a triage works from.
#[must_use]
pub fn render(history: &History, violation: &Violation) -> String {
    let mut out = format!(
        "global invariant `{}` broken in {} at seed {:#018x} ({}): {}\n",
        violation.invariant, history.scenario, history.seed, history.seed, violation.detail
    );
    let named = violation
        .records
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|at| history.records.get(at))
        .collect::<Vec<_>>();
    let excerpt: Vec<&Record> = if named.is_empty() {
        match &violation.session {
            Some(session) => {
                let mut tail = history
                    .records
                    .iter()
                    .rev()
                    .filter(|record| record.fact.session() == Some(session.as_str()))
                    .take(EXCERPT_RECORDS)
                    .collect::<Vec<_>>();
                tail.reverse();
                tail
            }
            None => Vec::new(),
        }
    } else {
        named
    };
    if !excerpt.is_empty() {
        out.push_str("  trace excerpt:\n");
        for record in excerpt {
            let fact = serde_json::to_string(&record.fact).unwrap_or_default();
            out.push_str(&format!("    #{:<5} {fact}\n", record.at));
        }
    }
    if !violation.rows.is_empty() {
        out.push_str("  store rows:\n");
        for row in &violation.rows {
            out.push_str(&format!("    {row}\n"));
        }
    }
    out
}

/// Check the history of a run on one engine: `recorder`'s facts and the
/// engine's final store.
pub async fn check_engine(
    scenario: impl Into<String>,
    seed: u64,
    recorder: &HistoryRecorder,
    engine: &crate::backend::SimEngine,
) -> Result<Report, String> {
    let mut history = History::new(scenario, seed);
    history.extend_from(recorder);
    capture_engines(
        &mut history,
        std::iter::once(("engine".to_owned(), engine.restate())),
    )
    .await?;
    Ok(check(&history))
}

/// How long the end of a history waits for one session's drive to settle.
const SETTLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The end of a history on `engine`: every session's drive has settled,
/// with the scope closes its roots owed. A send answers at its root's final
/// commit, before the root's scope closes (FIG-3979), so a store read at
/// that point sees a close still in flight. A drive that does not settle
/// within [`SETTLE_LIMIT`] (a turn the scenario leaves parked) is judged as
/// it stands.
async fn settle_drives(engine: &lash_restate_test::RestateTestBackend) -> Result<(), String> {
    let sessions = lash_sqlite_store::testing::read_rows_for_testing(
        engine.stores(),
        lash_sqlite_store::SqliteDatabase::DurableCore,
        "SELECT session_id FROM session_meta ORDER BY session_id",
    )?;
    for row in sessions {
        let Some((_, serde_json::Value::String(session))) = row.into_iter().next() else {
            continue;
        };
        let session = lash_core::SessionId::from(session);
        let _ = tokio::time::timeout(SETTLE_LIMIT, engine.settle_session_drive(&session)).await;
    }
    Ok(())
}

/// Settle every session's drive on each of `engines`, then capture every
/// store they wrote, with transcripts, into `history`.
pub async fn capture_engines<'a>(
    history: &mut History,
    engines: impl IntoIterator<Item = (String, &'a lash_restate_test::RestateTestBackend)>,
) -> Result<(), String> {
    for (label, engine) in engines {
        settle_drives(engine).await?;
        let now_ms = engine.server().now_ms();
        history.now_ms = Some(history.now_ms.map_or(now_ms, |at| at.max(now_ms)));
        history
            .capture_store_with_transcripts(label, engine.stores())
            .await?;
    }
    Ok(())
}

/// The global invariants over a crash-matrix or soak world whose end state
/// held: its final store after one more recovery pass, and after every claim
/// that store still held has lapsed and been retaken, judged under
/// `scenario` and the world's seed. Each failing violation is one rendered
/// line; quarantined ones are printed. A world on a live engine has no store
/// this can read, and is not judged.
pub async fn check_crash_world(
    world: &crate::crash_matrix::world::CrashWorld,
    scenario: &str,
) -> Vec<String> {
    let Ok(double) = world.double() else {
        return Vec::new();
    };
    // One more recovery pass, so the history ends after the relay had its
    // chance at everything the run armed, its last step's writes included.
    // A pass that fails leaves the history judged as one no pass ended.
    let relayed = world.tick().await.is_ok();
    world.quiesce().await;
    let mut history = match capture_crash_world(world, double, scenario, relayed).await {
        Ok(history) => history,
        Err(error) => return vec![error],
    };
    // A claim still held once every pass has quiesced is a dead claimant's:
    // its deployment died inside the pass that took it. Nobody may retake it
    // before it lapses (ADR 0109 §1.4), so the history ends only after the
    // recovery pass past the last lapse, which retakes it; what that pass
    // leaves claimed is judged.
    if let Some(lapse_ms) = history
        .last_claim_lapse_ms()
        .filter(|lapse_ms| *lapse_ms > world.now_ms())
    {
        // A tick moves the clock at least 90 % of `TICK`.
        let tick_ms = crate::crash_matrix::TICK.as_millis() as u64;
        let ticks = lapse_ms
            .saturating_sub(world.now_ms())
            .div_ceil(tick_ms - tick_ms / 10);
        let mut relayed = relayed;
        for _ in 0..ticks {
            if world.now_ms() >= lapse_ms {
                break;
            }
            relayed = world.tick().await.is_ok();
        }
        world.quiesce().await;
        history = match capture_crash_world(world, double, scenario, relayed).await {
            Ok(history) => history,
            Err(error) => return vec![error],
        };
    }
    let report = check(&history);
    report.print_quarantined();
    report.rendered[..report.violations.len()].to_vec()
}

/// `world`'s stores as they stand now, as a history of `scenario`.
async fn capture_crash_world(
    world: &crate::crash_matrix::world::CrashWorld,
    double: &lash_restate_test::RestateTestBackend,
    scenario: &str,
    relayed: bool,
) -> Result<History, String> {
    let mut history = History::new(scenario, world.seed());
    history.extend_from(world.history());
    history.relay_ran = relayed;
    history.now_ms = Some(world.now_ms());
    history
        .capture_store_with_transcripts("engine", double.stores())
        .await
        .map_err(|error| format!("capture the history for the global invariants: {error}"))?;
    Ok(history)
}
