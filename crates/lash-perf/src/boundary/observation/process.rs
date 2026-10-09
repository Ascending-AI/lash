//! Process observation: the language-observation dispatcher, the process
//! feed's subscribe/recover/reconcile path, bursts, replicas and the roster.
use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use futures_util::StreamExt as _;
use lash::process::{ProcessObservationStream, ProcessObservationStreamItem};
use lash_core::{
    ProcessObservationCursor, ProcessObservationEventPayload, ProcessObservationReplacement,
};

use super::super::{Args, Case, Meter, Receipt};
use super::{
    BLANK_SOURCE, Flavor, Fleet, Node, QUIET_SOURCE, commit_source, publish, settled, start,
    within, within_population,
};

const SURFACE: &str = "facade-workflow-process+process-feed";

pub(super) async fn run(args: &Args) -> Result<Receipt> {
    let meter = Meter::default();
    let nodes = match args.case {
        Case::ProcessConvergence | Case::ProcessReconcile => 2,
        // The quiet observer sits on a second replica where replicas share
        // one replay store.
        Case::ProcessBurst if args.postgres_url.is_some() => 2,
        _ => 1,
    };
    let stale_reader = matches!(args.case, Case::ProcessReconcile);
    let fleet = Fleet::open(args, &meter, nodes, Flavor::Workflow, |builder, index| {
        // The stalled consumer's node executes nothing, so its replay holds
        // only what its own feed reconciles into it.
        Ok(builder.serve_sessions(!(stale_reader && index == 1)))
    })
    .await?;
    let result = match args.case {
        Case::ProcessDispatcher => Box::pin(dispatcher(args, &fleet, &meter)).await,
        Case::ProcessFeeds => Box::pin(feeds(args, &fleet, &meter)).await,
        Case::ProcessBurst => Box::pin(burst(args, &fleet, &meter)).await,
        Case::ProcessConvergence => Box::pin(convergence(args, &fleet, &meter)).await,
        Case::ProcessReconcile => Box::pin(reconcile(args, &fleet, &meter)).await,
        Case::ProcessRoster => Box::pin(roster(args, &fleet, &meter)).await,
        other => Err(anyhow::anyhow!(
            "{other:?} is not a process observation workload"
        )),
    };
    let store = fleet.store;
    fleet.close().await?;
    let (counters, evidence) = result?;
    Ok(Receipt::measured(
        args.case,
        SURFACE,
        store,
        args.operations,
        &meter,
        counters,
        evidence,
    ))
}

type Outcome = Result<(serde_json::Value, serde_json::Value)>;

/// What one feed delivered until its process's terminal fact.
#[derive(Default, serde::Serialize)]
struct Delivered {
    language: usize,
    step_body: usize,
    committed: usize,
    gaps: BTreeMap<String, usize>,
    terminal: bool,
    /// The error a feed ended with before its terminal, when it did. A feed
    /// that errors is over; the workload records it and carries on.
    feed_error: Option<String>,
    /// When each committed sequence reached this consumer, by event or by a
    /// gap's replacement view.
    #[serde(skip)]
    arrivals: BTreeMap<u64, Instant>,
}

impl Delivered {
    fn gap_count(&self) -> usize {
        self.gaps.values().sum()
    }
}

/// Drain `feed` until the process's terminal fact, by event or by a gap's
/// replacement view, or until the feed ends with an error. Each item's wait
/// is one sample of `boundary`.
async fn follow(
    feed: &mut ProcessObservationStream,
    meter: &Meter,
    boundary: &str,
) -> Result<Delivered> {
    let mut delivered = Delivered::default();
    while !delivered.terminal {
        let start = Instant::now();
        let item = match within_population("process feed item", feed.next()).await? {
            Some(Ok(item)) => item,
            Some(Err(error)) => {
                delivered.feed_error = Some(error.to_string());
                break;
            }
            None => {
                delivered.feed_error = Some("the feed ended before its terminal".into());
                break;
            }
        };
        meter.sample(boundary, start.elapsed());
        match item {
            ProcessObservationStreamItem::Event(event) => match &event.payload {
                ProcessObservationEventPayload::LanguageExecution(_) => delivered.language += 1,
                ProcessObservationEventPayload::StepBodyStarted(_) => delivered.step_body += 1,
                ProcessObservationEventPayload::Committed { event } => {
                    delivered.committed += 1;
                    delivered.arrivals.insert(event.sequence, Instant::now());
                    delivered.terminal = matches!(
                        event.fact,
                        lash::process::ProcessLifecycleFact::Terminal { .. }
                    );
                }
            },
            ProcessObservationStreamItem::Gap { replacement, .. } => match replacement {
                ProcessObservationReplacement::Replaced { view, cause, .. } => {
                    *delivered.gaps.entry(format!("{cause:?}")).or_default() += 1;
                    delivered
                        .arrivals
                        .insert(view.process.last_event_sequence, Instant::now());
                    delivered.terminal = view.process.terminal().is_some();
                }
                ProcessObservationReplacement::Ended(end) => {
                    *delivered.gaps.entry(format!("{end:?}")).or_default() += 1;
                    delivered.terminal = true;
                }
            },
        }
    }
    Ok(delivered)
}

/// Snapshot `process` on `node`, then follow its feed to the terminal on a
/// task of its own. The snapshot's cursor is answered with what was
/// delivered. Snapshots are taken one at a time: each reads the process's
/// document through the node's VM workers, whose queue is small.
async fn observe(
    node: &Node,
    process: &lash::ProcessId,
    meter: &Meter,
    boundary: &'static str,
) -> Result<tokio::task::JoinHandle<Result<(ProcessObservationCursor, Delivered)>>> {
    let observed = node.core.processes().observe(process);
    let start = Instant::now();
    let snapshot = within("process snapshot", observed.snapshot()).await??;
    meter.sample("process.feed.snapshot", start.elapsed());
    let meter = meter.clone();
    Ok(tokio::spawn(async move {
        let mut feed = observed.subscribe_and_recover(snapshot.cursor.clone());
        let delivered = follow(&mut feed, &meter, boundary).await?;
        Ok((snapshot.cursor, delivered))
    }))
}

async fn durable_sequence(node: &Node, process: &lash::ProcessId) -> Result<u64> {
    Ok(node
        .core
        .processes()
        .get(process)
        .await?
        .context("the process is retained")?
        .last_event_sequence)
}

fn replay_counts(fleet: &Fleet) -> Vec<super::probes::ProcessReplayCounts> {
    fleet
        .nodes
        .iter()
        .map(|node| node.process_replay.counts())
        .collect()
}

/// `--callers` processes run `--operations` committed steps each on one node,
/// every one followed by a feed. The receipt states what the dispatcher
/// handed the replay store per round trip and how far its ingress fell
/// behind.
async fn dispatcher(args: &Args, fleet: &Fleet, meter: &Meter) -> Outcome {
    let node = &fleet.nodes[0];
    let published = publish(&node.core, &commit_source(args.operations)).await?;
    let window = Instant::now();
    let mut feeds = Vec::new();
    let mut processes = Vec::new();
    for n in 0..args.callers {
        let process = start(&node.core, &published, &format!("dispatcher-{n}")).await?;
        feeds.push(observe(node, &process, meter, "process.feed.next").await?);
        processes.push(process);
    }
    let (mut gaps, mut feed_errors) = (0, BTreeMap::<String, usize>::new());
    for feed in feeds {
        let (_, delivered) = feed.await??;
        gaps += delivered.gap_count();
        if let Some(error) = delivered.feed_error {
            *feed_errors.entry(error).or_default() += 1;
        }
    }
    for process in &processes {
        settled(&node.core, process).await?;
    }
    let counts = node.process_replay.counts();
    let provisional = counts.language_drafts + counts.step_body_drafts;
    meter.window(
        "process.dispatcher.observations",
        provisional as usize,
        window,
    );
    ensure!(provisional > 0, "the dispatcher published no observation");
    let seconds = window.elapsed().as_secs_f64();
    Ok((
        serde_json::json!({
            "observations_per_second": provisional as f64 / seconds,
            "store_round_trips_per_observation":
                counts.publish_calls as f64 / counts.drafts.max(1) as f64,
            "queue_depth_deepest": counts.deepest_backlog,
            "store_invalidations": counts.invalidate_all,
            "feed_gaps": gaps,
            "feeds_ended_with_error": feed_errors,
            "replay": counts,
        }),
        serde_json::json!({
            "processes": args.callers,
            "committed_steps_per_process": args.operations,
            "feed_gaps": gaps,
            "feeds_ended_with_error": feed_errors.values().sum::<usize>(),
        }),
    ))
}

/// One process commits `--operations` steps while `--callers` feeds follow
/// it. The receipt states how often each commit was published and what the
/// store read while the feeds were open; a feed from a stale cursor then
/// recovers.
async fn feeds(args: &Args, fleet: &Fleet, meter: &Meter) -> Outcome {
    let node = &fleet.nodes[0];
    let published = publish(&node.core, &commit_source(args.operations)).await?;
    // SQLite statements are counted by the store's own witness; another
    // collector in this OS process leaves the field absent.
    let witness = lash_core::perf_witness::Collector::install().ok();
    let window = Instant::now();
    let process = start(&node.core, &published, "feeds").await?;
    let mut open = Vec::new();
    for _ in 0..args.callers {
        open.push(observe(node, &process, meter, "process.feed.next").await?);
    }
    let mut delivered = Vec::new();
    let mut stale = None;
    for feed in open {
        let (cursor, feed) = feed.await??;
        stale.get_or_insert(cursor);
        delivered.push(feed);
    }
    settled(&node.core, &process).await?;
    let committed: usize = delivered.iter().map(|feed| feed.committed).sum();
    meter.window("process.feeds.committed_deliveries", committed, window);
    let commits = durable_sequence(node, &process).await?;
    let counts = node.process_replay.counts();
    let statements = witness.as_ref().map(|witness| witness.snapshot());
    drop(witness);

    let start = Instant::now();
    let observed = node.core.processes().observe(&process);
    let mut feed = observed.subscribe_and_recover(stale.context("one feed")?);
    let recovered = follow(&mut feed, meter, "process.feed.recover.next").await?;
    meter.record("process.feed.recover", 1, start);

    let gaps: usize = delivered.iter().map(Delivered::gap_count).sum();
    let errored = delivered
        .iter()
        .filter(|feed| feed.feed_error.is_some())
        .count();
    Ok((
        serde_json::json!({
            "feeds_ended_with_error": errored,
            "durable_commits": commits,
            "committed_publications": counts.committed_drafts,
            "committed_publications_per_commit": counts.committed_drafts as f64 / commits as f64,
            "committed_redeliveries": counts.redelivered,
            "sqlite_statements": statements.as_ref().map(|s| s.sql_statements),
            "sqlite_selects": statements.as_ref().and_then(|s| s.sql_statements_by_verb.get("select").copied()),
            "sqlite_statements_per_commit":
                statements.as_ref().map(|s| s.sql_statements as f64 / commits as f64),
            "feed_gaps": gaps,
            "replay": counts,
            "feeds": delivered,
            "recovered": recovered,
        }),
        serde_json::json!({
            "open_feeds": args.callers,
            "durable_commits": commits,
            "commit_rate_per_second": commits as f64 / window.elapsed().as_secs_f64(),
            "recover_gaps": recovered.gap_count(),
        }),
    ))
}

/// `--callers` processes burst `--operations` committed steps each while one
/// quiet process waits, followed from the last replica. The receipt counts
/// the store-wide invalidations and the gaps, each a resnapshot, that
/// reached every feed.
async fn burst(args: &Args, fleet: &Fleet, meter: &Meter) -> Outcome {
    let node = &fleet.nodes[0];
    let observer = fleet.nodes.last().context("a node")?;
    let chatty = publish(&node.core, &commit_source(args.operations)).await?;
    let quiet = publish(&node.core, QUIET_SOURCE).await?;
    let quiet = start(&node.core, &quiet, "burst-quiet").await?;
    let quiet_feed = observe(observer, &quiet, meter, "process.feed.quiet.next").await?;
    // The quiet feed is attached before the burst starts.
    tokio::time::timeout(super::SETTLE, async {
        while observer.process_replay.counts().subscribe_calls == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .context("the quiet feed subscribes")?;
    let window = Instant::now();
    let mut feeds = Vec::new();
    let mut processes = Vec::new();
    for n in 0..args.callers {
        let process = start(&node.core, &chatty, &format!("burst-{n}")).await?;
        feeds.push(observe(node, &process, meter, "process.feed.next").await?);
        processes.push(process);
    }
    let mut chatty_gaps = BTreeMap::<String, usize>::new();
    let mut feed_errors = BTreeMap::<String, usize>::new();
    for feed in feeds {
        let (_, delivered) = feed.await??;
        for (cause, count) in delivered.gaps {
            *chatty_gaps.entry(cause).or_default() += count;
        }
        if let Some(error) = delivered.feed_error {
            *feed_errors.entry(error).or_default() += 1;
        }
    }
    for process in &processes {
        settled(&node.core, process).await?;
    }
    let counts = replay_counts(fleet);
    let provisional: u64 = counts
        .iter()
        .map(|c| c.language_drafts + c.step_body_drafts)
        .sum();
    meter.window("process.burst.observations", provisional as usize, window);
    node.core
        .processes()
        .cancel(&quiet, node.core.effect_host())
        .await?;
    let (_, quiet_delivered) = quiet_feed.await??;
    let invalidations: u64 = counts.iter().map(|c| c.invalidate_all).sum();
    let resnapshots = chatty_gaps.values().sum::<usize>() + quiet_delivered.gap_count();
    Ok((
        serde_json::json!({
            "store_invalidations": invalidations,
            "bursting_feed_gaps": chatty_gaps,
            "quiet_feed_gaps": quiet_delivered.gaps,
            "quiet_feed_error": quiet_delivered.feed_error,
            "bursting_feeds_ended_with_error": feed_errors,
            "resnapshots": resnapshots,
            "replay_by_node": counts,
        }),
        serde_json::json!({
            "bursting_processes": args.callers,
            "committed_steps_per_process": args.operations,
            "quiet_observer_node": fleet.nodes.len() - 1,
            "resnapshots": resnapshots,
        }),
    ))
}

/// One process commits `--operations` steps while a feed on each of two
/// replicas follows it. A fact's convergence lag is the time between its
/// arrival at the two consumers.
async fn convergence(args: &Args, fleet: &Fleet, meter: &Meter) -> Outcome {
    let node = &fleet.nodes[0];
    let published = publish(&node.core, &commit_source(args.operations)).await?;
    let window = Instant::now();
    let process = start(&node.core, &published, "convergence").await?;
    let mut open = Vec::new();
    for node in &fleet.nodes {
        open.push(observe(node, &process, meter, "process.feed.next").await?);
    }
    let mut delivered = Vec::new();
    for feed in open {
        delivered.push(feed.await??.1);
    }
    settled(&node.core, &process).await?;
    let (first, second) = (&delivered[0], &delivered[1]);
    let (mut both, mut second_trailed) = (0, 0);
    for (sequence, at) in &first.arrivals {
        if let Some(other) = second.arrivals.get(sequence) {
            both += 1;
            second_trailed += usize::from(other > at);
            meter.sample(
                "process.convergence.lag",
                if other > at {
                    *other - *at
                } else {
                    *at - *other
                },
            );
        }
    }
    meter.window("process.convergence.facts", both, window);
    Ok((
        serde_json::json!({
            "feed_errors_by_replica": [first.feed_error.clone(), second.feed_error.clone()],
            "facts_on_both_replicas": both,
            "facts_second_replica_trailed": second_trailed,
            "gaps_by_replica": [first.gaps.clone(), second.gaps.clone()],
            "replay_by_node": replay_counts(fleet),
        }),
        serde_json::json!({
            "replicas": 2,
            "durable_commits": durable_sequence(node, &process).await?,
            "converged_facts": both,
        }),
    ))
}

/// A held process whose facts are appended through the process registry
/// port, with no execution: `facts` wait facts and then its terminal. Long
/// histories are written this way because an executed process slows with its
/// own length.
async fn append_facts(
    node: &Node,
    facts: usize,
    snapshot: impl AsyncFnOnce(&lash::ProcessId) -> Result<lash_core::ProcessObservation>,
    meter: &Meter,
) -> Result<(lash::ProcessId, lash_core::ProcessObservation)> {
    use lash_core::{
        ProcessCompletionAuthority, ProcessEventAppendRequest, ProcessExecutionWriteAuthority,
        ProcessRegistration, ToolCallId, ToolId, WaitKind, WaitState,
    };
    lash_core::testing::process_execution_env_fixture(node.stores.process_env_store().as_ref())
        .await;
    let registry = node.stores.process_registry();
    let process = registry
        .register_process(
            ProcessRegistration::new(
                lash_core::testing::held_engine_input(serde_json::Value::Null),
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(lash_core::testing::process_execution_env_fixture_ref())),
        )
        .await?
        .id;
    let authority =
        ProcessExecutionWriteAuthority::invocation(process.clone(), "reconcile-workload")
            .bind_attempt(1);
    registry
        .record_first_started_with_authority(
            &process,
            authority.invocation_started().context("a bound attempt")?,
            &authority,
        )
        .await?;
    let held = snapshot(&process).await?;
    let window = Instant::now();
    for n in 0..facts {
        let wait = WaitState {
            since_ms: n as i64,
            kind: WaitKind::Call {
                call_id: ToolCallId::fixture(&format!("reconcile-{n}")),
                tool_id: ToolId::from("reconcile-workload"),
            },
            site: None,
        };
        registry
            .append_event_with_authority(
                &process,
                ProcessEventAppendRequest::wait_entered(&process, &wait),
                &authority,
            )
            .await?;
    }
    meter.window("process.registry.append", facts, window);
    registry
        .complete_process(
            &process,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            ProcessCompletionAuthority::workflow_key(&process),
        )
        .await?;
    Ok((process, held))
}

/// A consumer on a replica that executes nothing snapshots a process at its
/// start and stalls while `--operations` facts are committed: executed steps
/// on the other replica, or with `--workload registry-facts` facts appended
/// through the registry port. Its feed then recovers from the stale cursor;
/// the receipt counts the facts reconcile pushed into that replica's replay
/// window and whether the feed gapped anyway.
async fn reconcile(args: &Args, fleet: &Fleet, meter: &Meter) -> Outcome {
    let (node, stalled) = (&fleet.nodes[0], &fleet.nodes[1]);
    let appended = args.workload == "registry-facts";
    let (process, snapshot) = if appended {
        let stale = async |process: &lash::ProcessId| {
            let observed = stalled.core.processes().observe(process);
            Ok(within("process snapshot", observed.snapshot()).await??)
        };
        append_facts(node, args.operations, stale, meter).await?
    } else {
        let published = publish(&node.core, &commit_source(args.operations)).await?;
        let process = start(&node.core, &published, "reconcile").await?;
        let observed = stalled.core.processes().observe(&process);
        let snapshot = within("process snapshot", observed.snapshot()).await??;
        settled(&node.core, &process).await?;
        (process, snapshot)
    };
    let observed = stalled.core.processes().observe(&process);
    let held = snapshot
        .read_view
        .sequence()
        .context("the process is retained")?
        .as_u64();
    let durable = durable_sequence(node, &process).await?;
    let before = stalled.process_replay.counts();

    let start = Instant::now();
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    let recovered = follow(&mut feed, meter, "process.feed.recover.next").await?;
    meter.record("process.feed.recover", 1, start);
    meter.window("process.reconcile.recover", recovered.committed, start);
    let after = stalled.process_replay.counts();

    let pushed = after.committed_drafts - before.committed_drafts;
    let limits = lash::tracing::ObservationWorkLimits::standard();
    Ok((
        serde_json::json!({
            "held_sequence": held,
            "durable_sequence": durable,
            "facts_behind": durable - held,
            "facts_pushed_into_window": pushed,
            "window_events_per_process":
                lash_core::InMemoryProcessReplayStoreConfig::standard().max_events_per_process,
            "reconcile_fact_budget":
                limits.process_effect_fold_pages * limits.process_effect_fold_page_size.get(),
            "publications_before_recover": before.drafts,
            "recover_gaps": recovered.gaps,
            "committed_facts_delivered": recovered.committed,
            "stalled_replica_replay": after,
        }),
        serde_json::json!({
            "facts_behind": durable - held,
            "facts_pushed_into_window": pushed,
            "gapped_anyway": recovered.gap_count() > 0,
            "facts_committed_by": if appended { "process-registry-port" } else { "execution" },
        }),
    ))
}

/// `--operations` processes are registered and ended, `--callers` at a time.
/// The roster is then paged whole, paged under the running-only filter, which
/// selects none of them, and its change feed is scanned from the start.
async fn roster(args: &Args, fleet: &Fleet, meter: &Meter) -> Outcome {
    let node = &fleet.nodes[0];
    let processes = node.core.processes();
    let published = publish(&node.core, BLANK_SOURCE).await?;
    let window = Instant::now();
    let mut started = 0;
    while started < args.operations {
        let wave = args.callers.min(args.operations - started);
        futures_util::future::try_join_all((started..started + wave).map(|n| {
            let published = &published;
            async move {
                let process = start(&node.core, published, &format!("roster-{n}")).await?;
                settled(&node.core, &process).await
            }
        }))
        .await?;
        started += wave;
    }
    meter.window("process.roster.populate", started, window);

    let limit = NonZeroUsize::new(128).context("page size")?;
    let everything = lash_core::ProcessListFilter {
        status: lash_core::ProcessStatusFilter::Any,
        ..Default::default()
    };
    // The default filter selects running processes, and every one has ended.
    let nothing = lash_core::ProcessListFilter::default();
    let mut scans = serde_json::Map::new();
    for (name, boundary, filter) in [
        ("unfiltered", "process.roster.page", &everything),
        (
            "selecting_nothing",
            "process.roster.page.filtered",
            &nothing,
        ),
    ] {
        let window = Instant::now();
        let (mut pages, mut rows, mut empty_pages, mut continuation) = (0, 0, 0, None);
        loop {
            let at = Instant::now();
            let page = within("roster page", processes.list(filter, limit, continuation)).await??;
            meter.sample(boundary, at.elapsed());
            pages += 1;
            rows += page.processes.len();
            empty_pages += usize::from(page.processes.is_empty());
            continuation = page.continuation;
            if continuation.is_none() {
                break;
            }
        }
        meter.window(&format!("{boundary}s"), pages, window);
        scans.insert(
            name.into(),
            serde_json::json!({"pages": pages, "rows": rows, "empty_pages": empty_pages}),
        );
    }
    // A shared database's roster also holds what earlier populations left.
    ensure!(
        scans["unfiltered"]["rows"].as_u64() >= Some(args.operations as u64),
        "the roster scan lost processes: {scans:?}"
    );

    let window = Instant::now();
    let (mut pages, mut changes) = (0, 0);
    let mut cursor = lash_core::ProcessChangeCursor::initial();
    loop {
        let at = Instant::now();
        let page = within("change page", processes.changed_since(cursor, limit)).await??;
        meter.sample("process.changes.page", at.elapsed());
        pages += 1;
        changes += page.changes.len();
        if page.changes.is_empty() {
            break;
        }
        cursor = page.next;
    }
    meter.window("process.changes.pages", pages, window);
    Ok((
        serde_json::json!({
            "roster_size": args.operations,
            "page_limit": limit,
            "roster_scans": scans,
            "change_feed": {"pages": pages, "changes": changes},
        }),
        serde_json::json!({
            "roster_size": args.operations,
            "roster_rows_read": scans["unfiltered"]["rows"],
            "changes_read": changes,
        }),
    ))
}
