//! The held-frame rule (FIG-7211), kept out of the merge-path gates.
use super::history::TurnCost;
use lash_core_ids::perf_witness::history::{Component, Cost};
use serde::Serialize;
use std::collections::BTreeMap;

const LIMIT: f64 = 1.05;
#[derive(Debug, Serialize)]
pub(super) struct Comparison {
    pub component: String,
    pub metric: &'static str,
    pub per_held_byte: bool,
    pub early: f64,
    pub late: f64,
    pub ratio: f64,
    pub pass: bool,
}
#[derive(Debug, Serialize)]
pub(super) struct Verdict {
    pub pass: bool,
    pub comparisons: Vec<Comparison>,
    pub sawtooth: Vec<Sawtooth>,
}
#[derive(Debug, Serialize)]
pub(super) struct Sawtooth {
    pub before_turn: usize,
    pub after_turn: usize,
    pub before_frame: String,
    pub after_frame: String,
    pub switch_turn: Option<usize>,
    pub switch_request_held_bytes: Option<usize>,
    pub sql_before: BTreeMap<String, Cost>,
    pub sql_after: BTreeMap<String, Cost>,
    pub held_bytes_before: usize,
    pub held_bytes_after: usize,
    pub before: BTreeMap<Component, Cost>,
    pub after: BTreeMap<Component, Cost>,
    pub ownership_before: Option<super::history_ownership::Ownership>,
    pub ownership_after: Option<super::history_ownership::Ownership>,
    pub vm_before: BTreeMap<String, u64>,
    pub vm_after: BTreeMap<String, u64>,
}
fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}
fn compare(
    component: String,
    metric: &'static str,
    normalized: bool,
    early: f64,
    late: f64,
) -> Comparison {
    let pass = late <= early * LIMIT;
    Comparison {
        component,
        metric,
        per_held_byte: normalized,
        early,
        late,
        // Zero -> positive still fails. JSON has no infinity.
        ratio: if early == 0.0 {
            if late == 0.0 { 1.0 } else { f64::MAX }
        } else {
            late / early
        },
        pass,
    }
}
/// Compare equal-age windows after a frame change, and absolute depth windows
/// without it. Every required metric binds, including allocations and SQL work.
pub(super) fn evaluate(turns: &[TurnCost], reset_every: Option<usize>) -> anyhow::Result<Verdict> {
    anyhow::ensure!(turns.len() >= 60, "history gate needs at least 60 turns");
    let early: Vec<_> = turns
        .iter()
        .filter(|t| (10..=30).contains(&t.turn))
        .collect();
    let late_start = match reset_every {
        Some(k) => ((turns.len() - 31) / k) * k + 10,
        None => turns.len() - 20,
    };
    let late: Vec<_> = turns
        .iter()
        .filter(|t| (late_start..=late_start + 20).contains(&t.turn))
        .collect();
    anyhow::ensure!(
        early.len() == 21 && late.len() == 21,
        "incomplete history windows"
    );
    anyhow::ensure!(
        turns
            .iter()
            .all(|t| t.held_bytes > 0 && t.held_messages > 0 && !t.frame.is_empty()),
        "missing request history"
    );
    let mut comparisons = Vec::new();
    for component in Component::ALL {
        let normalized = reset_every.is_none() && component.holds_history();
        for (metric, pick) in metrics() {
            let value = |window: &[&TurnCost]| {
                median(
                    window
                        .iter()
                        .map(|t| {
                            pick(&t.components[&component])
                                / if normalized { t.held_bytes as f64 } else { 1.0 }
                        })
                        .collect(),
                )
            };
            comparisons.push(compare(
                format!("{component:?}"),
                metric,
                normalized,
                value(&early),
                value(&late),
            ));
        }
    }
    // Family-level work cannot hide behind a larger flat SQL aggregate.
    let families: std::collections::BTreeSet<_> = early
        .iter()
        .chain(&late)
        .flat_map(|t| t.sql.keys().cloned())
        .collect();
    for family in families {
        for (metric, pick) in metrics().into_iter().skip(3) {
            let value = |window: &[&TurnCost]| {
                median(
                    window
                        .iter()
                        .map(|t| pick(&t.sql.get(&family).copied().unwrap_or_default()))
                        .collect(),
                )
            };
            comparisons.push(compare(
                format!("sql:{family}"),
                metric,
                false,
                value(&early),
                value(&late),
            ));
        }
    }
    let vm_metrics: std::collections::BTreeSet<_> =
        turns.iter().flat_map(|t| t.vm.keys().cloned()).collect();
    for metric in vm_metrics {
        anyhow::ensure!(
            turns.iter().all(|t| t.vm.contains_key(&metric)),
            "incomplete VM population"
        );
        let value =
            |window: &[&TurnCost]| median(window.iter().map(|t| t.vm[&metric] as f64).collect());
        comparisons.push(compare(
            format!("cell_vm:{metric}"),
            "work",
            false,
            value(&early),
            value(&late),
        ));
    }
    if turns
        .iter()
        .any(|t| t.ownership.is_some() || t.vm.get("cell_executions").is_some_and(|n| *n > 0))
    {
        let schema = serde_json::to_value(lash_vm_client::WorkerWork::default())?;
        let seal_fields: Vec<_> = schema
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("worker work must be a record"))?
            .keys()
            .filter(|key| key.starts_with("seal_"))
            .collect();
        anyhow::ensure!(
            turns.iter().all(|t| t.ownership.is_some()
                && t.ownership
                    .as_ref()
                    .is_some_and(|o| o.live_heap_pages > 0 && o.live_heap_bytes > 0)
                && seal_fields.iter().all(|key| t.vm.contains_key(*key))
                && t.vm.get("seal_attempts").copied().unwrap_or(0) > 0),
            "incomplete checkpoint/seal population"
        );
        // Growing live heaps are allowed. Only their excess retained ownership
        // is normalized, never work examined or publication bytes.
        for (metric, pick) in [
            (
                "pages_per_live_page",
                (|o: &super::history_ownership::Ownership| (o.pinned_pages, o.live_heap_pages))
                    as fn(&_) -> _,
            ),
            (
                "bytes_per_live_byte",
                (|o: &super::history_ownership::Ownership| {
                    (o.pinned_bytes + o.staged_bytes, o.live_heap_bytes)
                }) as fn(&_) -> _,
            ),
        ] {
            let value = |window: &[&TurnCost]| {
                median(
                    window
                        .iter()
                        .filter_map(|t| t.ownership.as_ref())
                        .map(|o| {
                            let (pinned, live) = pick(o);
                            pinned as f64 / live.max(1) as f64
                        })
                        .collect(),
                )
            };
            comparisons.push(compare(
                "checkpoint_retention".into(),
                metric,
                false,
                value(&early),
                value(&late),
            ));
        }
        if reset_every.is_some() {
            for (metric, pick) in [
                (
                    "pinned_pages",
                    (|o: &super::history_ownership::Ownership| o.pinned_pages) as fn(&_) -> _,
                ),
                (
                    "pinned_bytes",
                    (|o: &super::history_ownership::Ownership| o.pinned_bytes) as fn(&_) -> _,
                ),
                (
                    "staged_bytes",
                    (|o: &super::history_ownership::Ownership| o.staged_bytes) as fn(&_) -> _,
                ),
                (
                    "superseded_pinned",
                    (|o: &super::history_ownership::Ownership| o.superseded_pinned) as fn(&_) -> _,
                ),
            ] {
                let value = |window: &[&TurnCost]| {
                    median(
                        window
                            .iter()
                            .filter_map(|t| t.ownership.as_ref())
                            .map(|o| pick(o) as f64)
                            .collect(),
                    )
                };
                comparisons.push(compare(
                    "checkpoint_retention".into(),
                    metric,
                    false,
                    value(&early),
                    value(&late),
                ));
            }
        }
    }
    let sawtooth: Vec<_> = turns
        .windows(2)
        .filter(|pair| pair[0].frame != pair[1].frame)
        .map(|pair| Sawtooth {
            before_turn: pair[0].turn,
            after_turn: pair[1].turn,
            before_frame: pair[0].frame.clone(),
            after_frame: pair[1].frame.clone(),
            switch_turn: pair[1].switch.then_some(pair[1].turn),
            switch_request_held_bytes: pair[1]
                .switch
                .then(|| pair[1].requests.first().map_or(0, |r| r.held_bytes)),
            sql_before: pair[0].sql.clone(),
            sql_after: pair[1].sql.clone(),
            held_bytes_before: pair[0].held_bytes,
            held_bytes_after: pair[1].held_bytes,
            before: pair[0].components.clone(),
            after: pair[1].components.clone(),
            ownership_before: pair[0].ownership.clone(),
            ownership_after: pair[1].ownership.clone(),
            vm_before: pair[0].vm.clone(),
            vm_after: pair[1].vm.clone(),
        })
        .collect();
    if let Some(k) = reset_every {
        anyhow::ensure!(
            sawtooth.len() == (turns.len() - 1) / k,
            "frame change did not change every scheduled frame"
        );
        anyhow::ensure!(
            sawtooth
                .iter()
                .all(|s| s.held_bytes_after < s.held_bytes_before),
            "frame change did not reset held history"
        );
    } else {
        anyhow::ensure!(sawtooth.is_empty(), "unexpected frame change");
    }
    Ok(Verdict {
        pass: comparisons.iter().all(|c| c.pass),
        comparisons,
        sawtooth,
    })
}
type Metric = (&'static str, fn(&Cost) -> f64);
fn metrics() -> [Metric; 6] {
    [
        ("cpu_ns", |c| c.cpu_ns as f64),
        ("allocations", |c| c.allocations as f64),
        ("allocated_bytes", |c| c.allocated_bytes as f64),
        ("statements", |c| c.statements as f64),
        ("vm_steps", |c| c.vm_steps as f64),
        ("rows", |c| c.rows as f64),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    // FIG-7230: small publication bytes cannot hide a scan or retained old
    // checkpoints. Missing populations refuse rather than looking like zero.
    #[test]
    fn checkpoint_gate_requires_work_and_bounds_retained_ownership() {
        let mut turns: Vec<_> = (1..=1000)
            .map(|turn| {
                let components = Component::ALL
                    .into_iter()
                    .map(|c| (c, Cost::default()))
                    .collect();
                let mut row = TurnCost::fixture(turn, components);
                row.ownership = Some(super::super::history_ownership::Ownership {
                    live_heap_pages: turn as u64,
                    live_heap_bytes: turn as u64 * 100,
                    pinned_pages: turn as u64,
                    pinned_bytes: turn as u64 * 100,
                    ..Default::default()
                });
                row.vm.insert("vm_steps".into(), 1);
                row
            })
            .collect();
        assert!(
            evaluate(&turns, None).is_err(),
            "missing seal population must refuse"
        );
        let fields = serde_json::to_value(lash_vm_client::WorkerWork::default()).unwrap();
        for row in &mut turns {
            for key in fields
                .as_object()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with("seal_"))
            {
                row.vm.insert(key.clone(), 10);
            }
        }
        assert!(
            evaluate(&turns, None).unwrap().pass,
            "live heap growth is allowed"
        );
        for row in &mut turns {
            row.vm.insert("seal_nodes_visited".into(), row.turn as u64);
        }
        let verdict = evaluate(&turns, None).unwrap();
        assert!(
            verdict
                .comparisons
                .iter()
                .any(|c| c.component == "cell_vm:seal_nodes_visited" && !c.pass)
        );
        for row in &mut turns {
            row.vm.insert("seal_nodes_visited".into(), 10);
            row.ownership.as_mut().unwrap().pinned_pages *= row.turn as u64;
        }
        let verdict = evaluate(&turns, None).unwrap();
        assert!(
            verdict
                .comparisons
                .iter()
                .any(|c| c.component == "checkpoint_retention" && !c.pass)
        );
        for row in &mut turns {
            let owner = row.ownership.as_mut().unwrap();
            owner.pinned_pages = owner.live_heap_pages;
            owner.staged_bytes = row.turn as u64 * owner.live_heap_bytes;
        }
        assert!(
            evaluate(&turns, None)
                .unwrap()
                .comparisons
                .iter()
                .any(|c| c.component == "checkpoint_retention"
                    && c.metric == "bytes_per_live_byte"
                    && !c.pass)
        );
    }
    // Named rule: non-history work cannot grow with turn count. A deliberate
    // linear violation must fail even when the request's held bytes also grow.
    #[test]
    fn linear_non_history_work_fails_the_held_state_gate() {
        let collector = lash_core_ids::perf_witness::history::Collector::install().unwrap();
        let mut turns = Vec::new();
        for turn in 1..=1000 {
            let mut components: BTreeMap<_, _> = Component::ALL
                .into_iter()
                .map(|c| (c, Cost::default()))
                .collect();
            let before = collector.snapshot()[&Component::OwnerRetention];
            {
                let _scope =
                    lash_core_ids::perf_witness::history::Scope::enter(Component::OwnerRetention);
                for n in 0..turn * 100 {
                    std::hint::black_box(n);
                }
            }
            let cost = collector.snapshot()[&Component::OwnerRetention].since(before);
            components.insert(Component::OwnerRetention, cost);
            turns.push(TurnCost::fixture(turn, components));
        }
        let verdict = evaluate(&turns, None).unwrap();
        assert!(!verdict.pass);
        eprintln!(
            "deliberate linear work: {:?}",
            verdict
                .comparisons
                .iter()
                .find(|c| c.component == "OwnerRetention" && c.metric == "cpu_ns")
                .unwrap()
        );
        assert!(
            verdict
                .comparisons
                .iter()
                .any(|c| c.component == "OwnerRetention" && c.metric == "cpu_ns" && !c.pass)
        );
        // The same slope is allowed for building and sending the held history.
        for row in &mut turns {
            let cost = row
                .components
                .insert(Component::OwnerRetention, Cost::default())
                .unwrap();
            row.components.insert(
                Component::RequestBuild,
                Cost {
                    cpu_ns: row.held_bytes as u64 * 1000,
                    ..cost
                },
            );
        }
        assert!(evaluate(&turns, None).unwrap().pass);
    }
    // FIG-7226 held-frame rule: equal-age resets must not hide growing VM or
    // SQL work. Held bytes may include a larger seed in the later frame.
    #[test]
    fn reset_gate_rejects_vm_and_sql_growth_with_seeded_history() {
        let mut turns: Vec<_> = (1..=300)
            .map(|turn| {
                let components = Component::ALL
                    .into_iter()
                    .map(|c| (c, Cost::default()))
                    .collect();
                let mut row = TurnCost::fixture(turn, components);
                let frame = (turn - 1) / 100;
                row.frame = frame.to_string();
                row.held_bytes = 4096 + ((turn - 1) % 100) * 100;
                row.vm.insert("vm_steps".into(), 100);
                row.sql.insert(
                    "fixture family".into(),
                    Cost {
                        vm_steps: 10,
                        ..Cost::default()
                    },
                );
                row
            })
            .collect();
        assert!(evaluate(&turns, Some(100)).unwrap().pass);
        for row in &mut turns[200..] {
            row.vm.insert("vm_steps".into(), 106);
            row.sql.get_mut("fixture family").unwrap().vm_steps = 11;
        }
        let verdict = evaluate(&turns, Some(100)).unwrap();
        assert!(!verdict.pass);
        assert!(
            verdict
                .comparisons
                .iter()
                .any(|c| c.component == "cell_vm:vm_steps" && !c.pass)
        );
        assert!(
            verdict
                .comparisons
                .iter()
                .any(|c| c.component == "sql:fixture family" && c.metric == "vm_steps" && !c.pass)
        );
        turns[100].frame = "0".into();
        // A missing scheduled reset is also a refusal (not merely a flat curve).
        for row in &mut turns[100..200] {
            row.frame = "0".into();
        }
        assert!(evaluate(&turns, Some(100)).is_err());
    }
}
