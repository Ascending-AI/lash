//! Code-mode figures comparable with other engines (FIG-7590): a cell's
//! start and wall time, each park's checkpoint and the cold resume after a
//! worker loss, kept VM side and store side apart. A workload opts in with
//! `"comparable": true`; its run then records the pool's bounded effect
//! phases and counts worker heap work. Nothing here is gated.
//!
//! See `docs/perf/instruments/replay-workloads.md#comparable-figures`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use lash_protocol_types::sync::MutexExt as _;
use serde::{Deserialize, Serialize};

/// Which side of the boundary a figure measures: only `vm` compares with an
/// in-process engine.
#[derive(Clone, Copy)]
enum Side {
    Vm,
    Store,
}

/// Every figure, in table order: name, side and unit.
const FIELDS: [(&str, Side, &str); 15] = [
    ("cell_start_ms", Side::Vm, "ms"),
    ("cell_vm_ms", Side::Vm, "ms"),
    ("cell_ms", Side::Vm, "ms"),
    ("tool_calls", Side::Vm, "calls"),
    ("parks", Side::Vm, "parks"),
    ("sealed_pages_per_park", Side::Vm, "pages"),
    ("sealed_bytes_per_park", Side::Vm, "bytes"),
    ("checkpoint_root_bytes", Side::Vm, "bytes"),
    ("resume_worker_start_ms", Side::Vm, "ms"),
    ("resume_run_ms", Side::Vm, "ms"),
    ("resume_ms", Side::Vm, "ms"),
    ("resume_pages_read", Side::Vm, "pages"),
    ("resume_bytes_read", Side::Vm, "bytes"),
    ("turn_wall_ms", Side::Store, "ms"),
    ("store_bytes_per_park", Side::Store, "bytes"),
];

/// Frames that hand a cell to the worker.
const SUBMIT: [&str; 4] = ["prepare", "documents", "start", "run"];
/// Frames that stop a slice at a statement the host observes.
const STOPPED: [&str; 2] = ["parked", "ended"];
/// A replacement worker's installation, part of its start.
const INSTALL: [&str; 2] = ["pool_start", "pool_started"];

/// One turn's figures. A figure the turn could not measure is absent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    pub turn: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub setup: bool,
    pub figures: BTreeMap<String, f64>,
}

/// A workload's `comparable` section.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comparable {
    /// The observers these figures ran under.
    pub observers: String,
    /// `vm` or `store`, per figure.
    pub sides: BTreeMap<String, String>,
    /// Per figure, the median over the steady turns that measured it.
    pub median: BTreeMap<String, f64>,
    pub turns: Vec<Turn>,
}

/// The counters a turn's figures are differences of.
#[derive(Clone, Copy)]
pub(super) struct Cut {
    events: usize,
    bodies: usize,
    counters: lash::vm::PoolCounters,
}

/// A fixture body entry the resume figures need.
struct Body {
    /// Effect-phase events recorded before the body ran.
    event: usize,
    pages_loaded: u64,
    kill: bool,
}

/// A fixture body's entry hook: its tool name and arguments.
type Hook = dyn Fn(&str, &serde_json::Value) + Send + Sync;

/// Fixture-body observations and the pool, for one comparable run.
#[derive(Default)]
pub(super) struct Observer {
    pool: Mutex<Option<lash::vm::WorkerPool>>,
    /// Every body entry; only those around a kill carry counters.
    bodies: Mutex<Vec<Option<Body>>>,
}

pub(super) const OBSERVERS: &str = "bounded effect-phase recorder on the worker pool; workers re-executed from lash-perf with heap-work counting; a fixture-body hook reading the pool at a kill and at the next body";

impl Observer {
    pub fn attach(&self, pool: Option<lash::vm::WorkerPool>) -> Result<()> {
        let pool = pool.context("comparable figures need a code-mode worker pool")?;
        ensure!(
            pool.effect_phases().is_some(),
            "comparable figures need effect phases"
        );
        *self.pool.lock_recover() = Some(pool);
        Ok(())
    }

    /// The hook each fixture body calls at entry. Bodies that neither kill
    /// nor follow a kill only count, so the hook stays off the turn's clock.
    pub fn hook(self: &Arc<Self>) -> Arc<Hook> {
        let observer = Arc::clone(self);
        Arc::new(move |_tool, args| {
            let kill = args
                .get("kill_workers")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let mut bodies = observer.bodies.lock_recover();
            let after_kill = bodies
                .last()
                .is_some_and(|body| body.as_ref().is_some_and(|b| b.kill));
            let body = (kill || after_kill).then(|| {
                let pool = observer.pool.lock_recover();
                Body {
                    event: pool
                        .as_ref()
                        .and_then(lash::vm::WorkerPool::effect_phases)
                        .map_or(0, |phases| phases.events.len()),
                    pages_loaded: pool.as_ref().map_or(0, |pool| {
                        pool.measurements().counters.worker_work.pages_loaded
                    }),
                    kill,
                }
            });
            bodies.push(body);
        })
    }

    pub fn cut(&self) -> Result<Cut> {
        let pool = self.pool.lock_recover();
        let pool = pool.as_ref().context("comparable pool is not attached")?;
        Ok(Cut {
            events: pool.effect_phases().map_or(0, |phases| phases.events.len()),
            bodies: self.bodies.lock_recover().len(),
            counters: pool.measurements().counters,
        })
    }

    /// The figures of the turn between `before` and now, whose round
    /// metrics are `metrics`.
    pub fn turn(
        &self,
        turn: usize,
        setup: bool,
        before: Cut,
        metrics: &BTreeMap<String, f64>,
    ) -> Result<Turn> {
        let after = self.cut()?;
        let phases = self
            .pool
            .lock_recover()
            .as_ref()
            .and_then(lash::vm::WorkerPool::effect_phases)
            .context("effect phases disappeared")?;
        ensure!(
            phases.dropped == 0,
            "the effect-phase recorder dropped {} events; comparable figures would be incomplete",
            phases.dropped
        );
        let events = &phases.events[before.events..after.events];
        let mut figures = BTreeMap::new();
        cell(events, &mut figures);
        let (b, a) = (&before.counters, &after.counters);
        let parks = received(events, "parked") as f64;
        figures.insert("parks".to_owned(), parks);
        let bodies = self.bodies.lock_recover();
        let turn_bodies = &bodies[before.bodies..after.bodies];
        figures.insert("tool_calls".to_owned(), turn_bodies.len() as f64);
        if parks > 0.0 {
            let sealed = a.worker_work.pages_sealed - b.worker_work.pages_sealed;
            let bytes = a.worker_work.seal_bytes_published - b.worker_work.seal_bytes_published;
            figures.insert("sealed_pages_per_park".to_owned(), sealed as f64 / parks);
            figures.insert("sealed_bytes_per_park".to_owned(), bytes as f64 / parks);
            if let Some(store) = metrics.get("store_bytes_written") {
                figures.insert("store_bytes_per_park".to_owned(), store / parks);
            }
        }
        let roots = a.checkpoint_roots - b.checkpoint_roots;
        if roots > 0 {
            figures.insert(
                "checkpoint_root_bytes".to_owned(),
                (a.checkpoint_root_bytes - b.checkpoint_root_bytes) as f64 / roots as f64,
            );
        }
        if let Some(kill) = turn_bodies
            .iter()
            .position(|body| body.as_ref().is_some_and(|body| body.kill))
        {
            let (Some(killed), Some(Some(next))) = (&turn_bodies[kill], turn_bodies.get(kill + 1))
            else {
                anyhow::bail!("turn {turn}: no tool body followed the worker kill");
            };
            let from = killed.event - before.events;
            resume(&events[from..], &mut figures)
                .with_context(|| format!("turn {turn}: cold resume"))?;
            figures.insert(
                "resume_pages_read".to_owned(),
                (next.pages_loaded - killed.pages_loaded) as f64,
            );
        }
        if let Some(wall) = metrics.get("wall_ms") {
            figures.insert("turn_wall_ms".to_owned(), *wall);
        }
        Ok(Turn {
            turn,
            setup,
            figures,
        })
    }
}

fn ms(nanos: u64) -> f64 {
    nanos as f64 / 1_000_000.0
}

fn is(event: &lash::vm::EffectPhase, phase: &str, kinds: &[&str]) -> bool {
    event.phase == phase && event.kind.is_some_and(|kind| kinds.contains(&kind))
}

fn received(events: &[lash::vm::EffectPhase], kind: &str) -> usize {
    events
        .iter()
        .filter(|event| is(event, "receive", &[kind]))
        .count()
}

/// Cell start, from the first frame that hands the cell over to the first
/// slice that stops at a statement; the time the host spent in worker
/// exchanges through the cell's end; and the cell's wall time.
fn cell(events: &[lash::vm::EffectPhase], figures: &mut BTreeMap<String, f64>) {
    let Some(submit) = events.iter().position(|e| is(e, "send", &SUBMIT)) else {
        return;
    };
    let start = events[submit].start_ns;
    if let Some(first) = events[submit..].iter().find(|e| is(e, "receive", &STOPPED)) {
        figures.insert("cell_start_ms".to_owned(), ms(first.end_ns - start));
    }
    let Some(end) = events.iter().rposition(|e| is(e, "receive", &["ended"])) else {
        return;
    };
    figures.insert("cell_ms".to_owned(), ms(events[end].end_ns - start));
    let exchanges: u64 = events[submit..=end]
        .iter()
        .filter(|e| (e.phase == "send" || e.phase == "receive") && !is(e, e.phase, &INSTALL))
        .map(|e| e.end_ns - e.start_ns)
        .sum();
    figures.insert("cell_vm_ms".to_owned(), ms(exchanges));
}

/// The worker the run resumes on after a kill: the one it sends its first
/// frame to, which must have started after the kill. Its start through
/// installation, then from that frame to the first slice that stops at a
/// statement, and the bytes it was sent in that window.
fn resume(events: &[lash::vm::EffectPhase], figures: &mut BTreeMap<String, f64>) -> Result<()> {
    let first = events
        .iter()
        .position(|e| e.phase == "send" && !is(e, "send", &INSTALL))
        .context("the run was never sent to a worker after the kill")?;
    let pid = events[first].pid;
    let spawn = events[..first]
        .iter()
        .find(|e| e.pid == pid && e.phase == "spawn_to_ready")
        .context("the run resumed on a worker that started before the kill")?;
    let installed = events[..first]
        .iter()
        .find(|e| e.pid == pid && is(e, "receive", &["pool_started"]))
        .map_or(spawn.end_ns, |e| e.end_ns);
    let stopped = first
        + events[first..]
            .iter()
            .position(|e| e.pid == pid && is(e, "receive", &STOPPED))
            .context("the resumed run never reached a statement")?;
    let worker_start = installed - spawn.start_ns;
    let run = events[stopped].end_ns - events[first].start_ns;
    let bytes: usize = events[first..=stopped]
        .iter()
        .filter(|e| e.pid == pid && e.phase == "send")
        .filter_map(|e| e.bytes)
        .sum();
    figures.insert("resume_worker_start_ms".to_owned(), ms(worker_start));
    figures.insert("resume_run_ms".to_owned(), ms(run));
    figures.insert("resume_ms".to_owned(), ms(worker_start + run));
    figures.insert("resume_bytes_read".to_owned(), bytes as f64);
    Ok(())
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        f64::midpoint(values[mid - 1], values[mid])
    } else {
        values[mid]
    })
}

/// The section for `turns`, with medians over the steady ones.
pub(super) fn section(turns: Vec<Turn>) -> Comparable {
    let median = FIELDS
        .iter()
        .filter_map(|(name, _, _)| {
            let values = turns
                .iter()
                .filter(|turn| !turn.setup)
                .filter_map(|turn| turn.figures.get(*name).copied())
                .collect();
            median(values).map(|value| ((*name).to_owned(), (value * 1_000.0).round() / 1_000.0))
        })
        .collect();
    Comparable {
        observers: OBSERVERS.to_owned(),
        sides: FIELDS
            .iter()
            .map(|(name, side, _)| {
                let side = match side {
                    Side::Vm => "vm",
                    Side::Store => "store",
                };
                ((*name).to_owned(), side.to_owned())
            })
            .collect(),
        median,
        turns,
    }
}

/// One table: a row per figure, a column per comparable workload.
pub fn table(workloads: &[(&str, &Comparable)]) -> String {
    if workloads.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "comparable (median over steady turns; vm side compares with an in-process engine)\n",
    );
    out.push_str(&format!("  {:<24} {:<6} {:<6}", "figure", "side", "unit"));
    for (name, _) in workloads {
        out.push_str(&format!(" {name:>24}"));
    }
    out.push('\n');
    for (field, side, unit) in FIELDS {
        out.push_str(&format!(
            "  {field:<24} {:<6} {unit:<6}",
            match side {
                Side::Vm => "vm",
                Side::Store => "store",
            }
        ));
        for (_, comparable) in workloads {
            out.push_str(&format!(
                " {:>24}",
                comparable
                    .median
                    .get(field)
                    .map_or_else(|| "-".to_owned(), |value| format!("{value:.3}"))
            ));
        }
        out.push('\n');
    }
    out
}
