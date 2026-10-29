//! The explicit cancellation law's durable-record and external-world oracle.
use super::*;

impl CellTurn {
    pub(super) async fn cancellation_laws(
        &self,
        nodes: &SimNodes,
        cut: Option<&Cut>,
        case: CancelCase,
    ) -> Vec<String> {
        let mut violations = turn_ended(nodes).await;
        if case.order == CommitOrder::FinalFirst {
            let view = lash_core_execution::store::SessionStore::new(
                self.backend().stores().session_store_factory(),
                session(),
            )
            .unwrap();
            let loaded = lash_core_execution::store::load_session_window_state(
                &view,
                lash_core_execution::store::WindowSelector::Current,
            )
            .await
            .unwrap()
            .unwrap();
            let count = loaded
                .state
                .plugin_state()
                .and_then(|state| state.plugins.get("tools"))
                .and_then(|namespace| namespace.values.get("count"))
                .and_then(Value::as_u64);
            if count != Some(2) {
                violations.push(format!(
                    "the retained final and its sibling must each increment once: {count:?}"
                ));
            }
        }

        let signals = self.world.cleanup_signals.lock_recover().clone();
        let expected =
            if case.explicit && case.order == CommitOrder::CancelFirst && case.hint.is_none() {
                vec![1]
            } else {
                Vec::new()
            };
        if cut.is_none() && signals.as_deref() != Some(expected.as_slice()) {
            violations.push(format!("the selected call must be signalled before cleanup, while its sibling stays live; observed {signals:?}; model saw {}", self.last_request()));
        }
        let observed_exception = self
            .world
            .entries()
            .values()
            .flatten()
            .any(|(tool, args)| tool == WRITE && written_x(args) == Some(4));
        if observed_exception != case.explicit {
            violations.push(format!(
                "the Python task's CancelledError handler ran={observed_exception}, explicit={}",
                case.explicit
            ));
        }
        if signals.as_ref().is_some_and(|signals| signals.contains(&2)) {
            violations.push(format!(
                "the sibling was signalled before cleanup: {signals:?}"
            ));
        }
        if case.hint.is_none() {
            let children = cancel_children(&self.backend()).await;
            let allowed = usize::from(case.order == CommitOrder::FinalFirst);
            if children.len() != allowed {
                violations.push(format!("protected declarations must drain, and cancelled declarations must never launch: {children:?}"));
            }
        }
        if let Some(hint) = case.hint {
            let children = self
                .world
                .cleanup_children
                .lock_recover()
                .clone()
                .unwrap_or_default();
            if children.len() != 2
                || children.iter().any(|(x, cancelled)| {
                    *cancelled
                        != (*x == 1
                            && hint == lash_core::CancelHint::CancelExternalWork
                            && case.explicit)
                })
            {
                violations.push(format!("the cleanup must see the selected CancelHint discharged, and no sibling cancel: {children:?}"));
            }
        }
        use lash_core_execution::runtime::actor::round::graph::RoundFact;
        use lash_core_store::checkpoint_graph::{DecodeLimits, GraphDecoder};
        use lash_durable::domain::RunFactKind as Kind;
        let selected = self.world.entries().iter().find_map(|(id, entries)| {
            entries
                .iter()
                .any(|(tool, args)| tool == SLOW && written_x(args) == Some(1))
                .then(|| id.clone())
        });
        let mut decisions = BTreeMap::new();
        let mut outcomes = BTreeMap::new();
        for write in nodes
            .script()
            .trace()
            .iter()
            .filter(|write| write.committed())
        {
            let mut decoder =
                GraphDecoder::new(write.facts.as_ref(), DecodeLimits::within_total(u64::MAX));
            for row in &write.facts.rows {
                let Some(call) = &row.call else { continue };
                if matches!(row.kind, Kind::XCancel | Kind::XFinal)
                    && decisions.insert(call.clone(), row.kind).is_some()
                {
                    violations.push(format!(
                        "call {call} committed a second final-or-cancel decision"
                    ));
                }
                if decisions.get(call) == Some(&Kind::XCancel)
                    && matches!(row.kind, Kind::XStart | Kind::Retry | Kind::XFinal)
                {
                    violations.push(format!(
                        "call {call} retried or declared after cancellation"
                    ));
                }
                if row.kind == Kind::XOutcome {
                    if outcomes.insert(call.clone(), ()).is_some() {
                        violations.push(format!("call {call} committed two outcomes"));
                    }
                    if Some(call) == selected.as_ref() {
                        let fact = RoundFact::load(&mut decoder, row.record).unwrap();
                        if let RoundFact::Outcome { output, .. } = fact {
                            let cancelled = matches!(
                                output.outcome,
                                lash_core_store::tool_run::AttemptOutcome::Cancelled { .. }
                            );
                            if cancelled != (case.order == CommitOrder::CancelFirst) {
                                violations.push(format!(
                                    "selected call's durable final disagrees with {case:?}: {:?}",
                                    output.outcome
                                ));
                            }
                        }
                    }
                }
            }
        }
        if let Some(selected) = selected {
            let expected = if case.order == CommitOrder::FinalFirst {
                Kind::XFinal
            } else {
                Kind::XCancel
            };
            let trace = nodes.script().trace();
            let decision_at = trace.iter().position(|write| {
                write.committed()
                    && write.facts.rows.iter().any(|row| {
                        row.call.as_ref() == Some(&selected)
                            && matches!(row.kind, Kind::XFinal | Kind::XCancel)
                    })
            });
            let cut_at = trace.iter().position(|write| write.cut.is_some());
            if decision_at.is_some_and(|decision| cut_at.is_none_or(|cut| decision <= cut))
                && self.world.entries().get(&selected).map(Vec::len) != Some(1)
            {
                violations.push(format!(
                    "the selected body ran again after its durable decision: {:?}",
                    self.world.entries().get(&selected)
                ));
            }
            if decisions.get(&selected) != Some(&expected) {
                violations.push(format!(
                    "selected call's decision was {:?}, expected {expected:?}",
                    decisions.get(&selected)
                ));
            }
        } else {
            violations.push("no selected call was admitted".to_owned());
        }
        violations
    }
}

pub(super) async fn run_law() {
    for case in [
        CancelCase::python(CommitOrder::CancelFirst, None, true),
        CancelCase::python(CommitOrder::FinalFirst, None, true),
        CancelCase::python(
            CommitOrder::CancelFirst,
            Some(lash_core::CancelHint::CancelExternalWork),
            true,
        ),
        CancelCase::python(
            CommitOrder::CancelFirst,
            Some(lash_core::CancelHint::Ignore),
            true,
        ),
        CancelCase::python(CommitOrder::CancelFirst, None, false),
    ] {
        let report = Matrix::new()
            .labels(&[
                CommitLabel::CELL_SNAPSHOT_ADMIT,
                CommitLabel::CALL_CANCEL,
                CommitLabel::CALL_FINAL,
                CommitLabel::ROUND_OUTCOME,
                CommitLabel::CELL_INJECT,
                CommitLabel::PROCESS_REGISTER,
                CommitLabel::PROCESS_CANCEL,
            ])
            .faults(&[Fault::Abort, Fault::CommitThenAbort, Fault::AckHidden])
            .horizon(Duration::from_secs(600))
            .run_test(|| CellTurn::new(Cell::ExplicitCancel(case), Dialect::SqliteMemory, None))
            .await;
        eprintln!(
            "{case:?}: {} crash cuts over {:?}",
            report.cells.len(),
            report.labels()
        );
        for label in [
            CommitLabel::CALL_FINAL,
            CommitLabel::CELL_SNAPSHOT_ADMIT,
            CommitLabel::ROUND_OUTCOME,
        ] {
            assert!(
                report.labels().contains(&label),
                "the law cut no {label} commit for {case:?}"
            );
        }
        if case.hint.is_some() {
            assert!(
                report.labels().contains(&CommitLabel::CELL_INJECT),
                "the law cut no discharge settlement for {case:?}"
            );
        }
        report.assert_held();
    }
}

pub(super) fn increment_state() -> lash::plugins::StateCommands {
    lash::plugins::StateCommands::new().apply("count", "increment", json!(1))
}

pub(super) fn increment_reducer() -> lash::plugins::StateReducer {
    Arc::new(|reduction| {
        Ok(Some(json!(
            reduction.current.and_then(Value::as_u64).unwrap_or(0)
                + reduction.input.as_u64().unwrap_or(0)
        )))
    })
}
