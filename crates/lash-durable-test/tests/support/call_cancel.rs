//! The explicit cancellation law's durable-record and external-world oracle.
#[path = "call_cancel_authority.rs"]
pub(super) mod authority;
use super::*;
use lash_vocabulary::identity::RunIdCoreSupport as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CancelSource {
    Host,
    Python,
    TypeScript,
    TypeScriptShared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CommitOrder {
    Cancel,
    Final,
    PlainFinal,
    TypedCancel,
}

impl CommitOrder {
    pub(super) fn final_wins(self) -> bool {
        matches!(self, Self::Final | Self::PlainFinal)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CancelCase {
    pub(super) source: CancelSource,
    pub(super) order: CommitOrder,
    pub(super) hint: Option<lash_core::CancelHint>,
    pub(super) explicit: bool,
}

/// The `x` a call was made with. A cell's numbers are its dialect's: a
/// TypeScript `1` reaches the tool as the number it is, written `1.0`.
pub(super) fn written_x(args: &Value) -> Option<i64> {
    let x = args["x"].as_f64()?;
    (x.fract() == 0.0).then_some(x as i64)
}

impl CancelCase {
    pub(super) fn slow_calls(self) -> usize {
        if self.source == CancelSource::TypeScriptShared {
            3
        } else {
            2
        }
    }

    pub(super) fn python(
        order: CommitOrder,
        hint: Option<lash_core::CancelHint>,
        explicit: bool,
    ) -> Self {
        Self {
            source: CancelSource::Python,
            order,
            hint,
            explicit,
        }
    }
}

impl CancelCase {
    fn typescript(order: CommitOrder, hint: Option<lash_core::CancelHint>, explicit: bool) -> Self {
        Self {
            source: CancelSource::TypeScript,
            order,
            hint,
            explicit,
        }
    }
}

pub(super) fn typescript_source(case: CancelCase) -> String {
    let preaborted = "const pre = new AbortController(); pre.abort(); pre.abort(); let refused = false; try { await tools.extSlow({ x: 0 }, { signal: pre.signal }); } catch (e) { refused = e.name === 'AbortError' && e.data.origin === 'model_requested'; } if (!refused || !pre.signal.aborted) { throw new Error('pre-aborted call did not reject'); }";
    let shared = case.source == CancelSource::TypeScriptShared;
    let extra_call = if shared {
        "const shared = tools.extSlow({ x: 5 }, { signal: controller.signal });"
    } else {
        ""
    };
    let extra_join = if shared {
        "let sharedCancelled = false; try { await shared; } catch (e) { sharedCancelled = e.name === 'AbortError' && e.data.origin === 'model_requested'; } if (!sharedCancelled) { throw new Error('shared signal lost its second call'); }"
    } else {
        ""
    };
    let cancel_and_join = if case.explicit {
        "controller.abort(); try { await a; } catch (e) { if (e.name === 'AbortError' && e.data && e.data.origin === 'model_requested') { await tools.extWrite({ x: 4 }); } }"
    } else {
        ""
    };
    format!(
        "{preaborted}\nconst controller = new AbortController();\nconst a = tools.extSlow({{ x: 1 }}, {{ signal: controller.signal }});\nconst b = tools.extSlow({{ x: 2 }});\n{extra_call}\nlet observerRefused = false; try {{ AbortController.prototype.abort.call({{ signal: controller.signal }}); }} catch (e) {{ observerRefused = e.name === 'TypeError'; }} if (!observerRefused || controller.signal.aborted) {{ throw new Error('a signal observer gained cancellation authority'); }}\nawait Promise.race([tools.extFast({{}}), a, b]);\n{cancel_and_join}\n{extra_join}\nawait tools.extWrite({{ x: 3 }});\nawait sleep(60000);\nconsole.log('cell stayed alive');"
    )
}

impl CellTurn {
    pub(super) async fn cancellation_laws(
        &self,
        nodes: &SimNodes,
        cut: Option<&Cut>,
        case: CancelCase,
    ) -> Vec<String> {
        let mut violations = turn_ended(nodes).await;
        violations.extend(self.world.authority_violations.lock_recover().clone());
        if case.order.final_wins() {
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

        let mut signals = self.world.cleanup_signals.lock_recover().clone();
        if let Some(signals) = &mut signals {
            signals.sort_unstable();
        }
        if matches!(
            case.source,
            CancelSource::TypeScript | CancelSource::TypeScriptShared
        ) && self
            .world
            .entries()
            .values()
            .flatten()
            .any(|(tool, args)| tool == SLOW && written_x(args) == Some(0))
        {
            violations.push("a pre-aborted signal admitted a body".to_owned());
        }
        let expected = if case.explicit && case.order == CommitOrder::Cancel && case.hint.is_none()
        {
            if case.source == CancelSource::TypeScriptShared {
                vec![1, 5]
            } else {
                vec![1]
            }
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
        let exception_expected = case.explicit
            && (case.source == CancelSource::Python
                || matches!(case.order, CommitOrder::Cancel | CommitOrder::TypedCancel));
        if observed_exception != exception_expected {
            violations.push(format!(
                "the guest cancellation handler ran={observed_exception}, explicit={}",
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
            let allowed = usize::from(case.order == CommitOrder::Final);
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
        let shared_call = self.world.entries().iter().find_map(|(id, entries)| {
            entries
                .iter()
                .any(|(tool, args)| tool == SLOW && written_x(args) == Some(5))
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
                if matches!(row.kind, Kind::XCancel | Kind::XFinal | Kind::XFinalOutcome)
                    && decisions.insert(call.clone(), row.kind).is_some()
                {
                    violations.push(format!(
                        "call {call} committed a second final-or-cancel decision"
                    ));
                }
                if decisions.get(call) == Some(&Kind::XCancel)
                    && matches!(
                        row.kind,
                        Kind::XStart | Kind::Retry | Kind::XFinal | Kind::XFinalOutcome
                    )
                {
                    violations.push(format!(
                        "call {call} retried or declared after cancellation"
                    ));
                }
                if case.source == CancelSource::Host
                    && case.explicit
                    && row.kind == Kind::XCancel
                    && Some(call) == selected.as_ref()
                    && let RoundFact::Cancel { cancellation, .. } =
                        RoundFact::load(&mut decoder, row.record).unwrap()
                {
                    let value = cancellation.to_json_value();
                    if value["origin"] != "operator_requested"
                        || value["message"] != "cancel the selected call"
                        || value["raw"]["request_id"] != "host-call-cancel"
                        || value["raw"]["origin"] != "host-race-loser"
                    {
                        violations.push(format!(
                            "the first host request's provenance was lost: {value}"
                        ));
                    }
                }
                if matches!(row.kind, Kind::XOutcome | Kind::XFinalOutcome) {
                    if outcomes.insert(call.clone(), ()).is_some() {
                        violations.push(format!("call {call} committed two outcomes"));
                    }
                    if Some(call) == selected.as_ref() || Some(call) == shared_call.as_ref() {
                        let fact = RoundFact::load(&mut decoder, row.record).unwrap();
                        if let RoundFact::Outcome { output, .. }
                        | RoundFact::FinalOutcome { output, .. } = fact
                        {
                            let cancelled = matches!(
                                output.outcome,
                                lash_core_store::tool_run::AttemptOutcome::Cancelled { .. }
                            );
                            if cancelled
                                != matches!(
                                    case.order,
                                    CommitOrder::Cancel | CommitOrder::TypedCancel
                                )
                            {
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
        if case.source == CancelSource::TypeScriptShared
            && shared_call.as_ref().is_none_or(|call| {
                decisions.get(call) != Some(&Kind::XCancel) || !outcomes.contains_key(call)
            })
        {
            violations.push(
                "the shared signal's second call lacks its durable cancellation and outcome"
                    .to_owned(),
            );
        }
        if let Some(selected) = selected {
            let expected = match case.order {
                CommitOrder::Final => Kind::XFinal,
                CommitOrder::PlainFinal => Kind::XFinalOutcome,
                CommitOrder::Cancel | CommitOrder::TypedCancel => Kind::XCancel,
            };
            let trace = nodes.script().trace();
            let decision_at = trace.iter().position(|write| {
                write.committed()
                    && write.facts.rows.iter().any(|row| {
                        row.call.as_ref() == Some(&selected)
                            && matches!(
                                row.kind,
                                Kind::XFinal | Kind::XFinalOutcome | Kind::XCancel
                            )
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

fn host(order: CommitOrder, hint: Option<lash_core::CancelHint>, explicit: bool) -> CancelCase {
    CancelCase {
        source: CancelSource::Host,
        order,
        hint,
        explicit,
    }
}

// One corpus drives the original complete SQLite selector and the PostgreSQL
// registrations. Every PG registration retains a whole matrix under the same
// watchdog, following crash_matrix.rs's service-case partition.
macro_rules! cancellation_cases {
    ($($name:ident => $case:expr;)*) => {
        pub(super) fn cases() -> Vec<CancelCase> { vec![$($case),*] }
        mod postgres {
            use super::*;
            $(
                #[tokio::test]
                #[ignore = "requires PostgreSQL"]
                async fn $name() {
                    run_cases(Dialect::Postgres,
                        Some(lash_postgres_store::testing::required_database_url()), &[$case]).await;
                }
            )*
        }
    };
}

cancellation_cases! {
    host_typed_cancel_on_postgres => host(CommitOrder::TypedCancel, None, true);
    host_cancel_on_postgres => host(CommitOrder::Cancel, None, true);
    host_final_on_postgres => host(CommitOrder::Final, None, true);
    host_plain_final_on_postgres => host(CommitOrder::PlainFinal, None, true);
    host_external_cancel_on_postgres => host(CommitOrder::Cancel, Some(lash_core::CancelHint::CancelExternalWork), true);
    host_ignore_on_postgres => host(CommitOrder::Cancel, Some(lash_core::CancelHint::Ignore), true);
    host_control_on_postgres => host(CommitOrder::Cancel, None, false);
    python_cancel_on_postgres => CancelCase::python(CommitOrder::Cancel, None, true);
    python_final_on_postgres => CancelCase::python(CommitOrder::Final, None, true);
    python_external_cancel_on_postgres => CancelCase::python(CommitOrder::Cancel, Some(lash_core::CancelHint::CancelExternalWork), true);
    python_ignore_on_postgres => CancelCase::python(CommitOrder::Cancel, Some(lash_core::CancelHint::Ignore), true);
    python_control_on_postgres => CancelCase::python(CommitOrder::Cancel, None, false);
}

mod typescript {
    use super::*;
    cancellation_cases! {
        typescript_cancel_on_postgres => CancelCase::typescript(CommitOrder::Cancel, None, true);
        typescript_final_on_postgres => CancelCase::typescript(CommitOrder::Final, None, true);
        typescript_plain_final_on_postgres => CancelCase::typescript(CommitOrder::PlainFinal, None, true);
        typescript_external_cancel_on_postgres => CancelCase::typescript(CommitOrder::Cancel, Some(lash_core::CancelHint::CancelExternalWork), true);
        typescript_ignore_on_postgres => CancelCase::typescript(CommitOrder::Cancel, Some(lash_core::CancelHint::Ignore), true);
        typescript_shared_on_postgres => CancelCase { source: CancelSource::TypeScriptShared, ..CancelCase::typescript(CommitOrder::Cancel, None, true) };
        typescript_control_on_postgres => CancelCase::typescript(CommitOrder::Cancel, None, false);
    }
}

pub(super) async fn run_typescript_law() {
    run_cases(Dialect::SqliteMemory, None, &typescript::cases()).await;
}

pub(super) async fn run_law() {
    run_cases(Dialect::SqliteMemory, None, &cases()).await;
}

async fn run_cases(dialect: Dialect, postgres_url: Option<String>, cases: &[CancelCase]) {
    let missing = authority::probe(ToolCallId::fixture("missing-authority")).await;
    eprintln!("missing authority refusals: {missing:?}");
    for &case in cases {
        // Each PostgreSQL cut owns a database and its pools. Keep the full
        // cut census within the private service's connection ceiling.
        let matrix = if matches!(dialect, Dialect::Postgres) {
            Matrix::new().parallelism(
                std::num::NonZeroUsize::new(4).expect("four PG cells per service slot"),
            )
        } else {
            Matrix::new()
        };
        let report = matrix
            .labels(&[
                CommitLabel::CELL_SNAPSHOT_ADMIT,
                CommitLabel::CALL_CANCEL_REQUEST,
                CommitLabel::CALL_CANCEL,
                CommitLabel::CALL_FINAL,
                CommitLabel::ROUND_OUTCOME,
                CommitLabel::CELL_INJECT,
                CommitLabel::PROCESS_REGISTER,
                CommitLabel::PROCESS_CANCEL,
            ])
            .faults(&[Fault::Abort, Fault::CommitThenAbort, Fault::AckHidden])
            .horizon(Duration::from_secs(600))
            .run_test(|| CellTurn::new(Cell::ExplicitCancel(case), dialect, postgres_url.clone()))
            .await;
        eprintln!(
            "{case:?}: {} crash cuts over {:?}",
            report.cells.len(),
            report.labels()
        );
        for label in [CommitLabel::CELL_SNAPSHOT_ADMIT, CommitLabel::ROUND_OUTCOME] {
            assert!(
                report.labels().contains(&label),
                "the law cut no {label} commit for {case:?}"
            );
        }
        if case.order == CommitOrder::Final {
            assert!(
                report.labels().contains(&CommitLabel::CALL_FINAL),
                "the law cut no {} commit for {case:?}",
                CommitLabel::CALL_FINAL
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
    assert!(missing.is_empty(), "{missing:?}");
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

/// Wait on durable park commits so readiness cannot keep the virtual clock
/// busy with PostgreSQL polling before the park batch window can elapse.
pub(super) async fn wait_pending_calls(world: &ExternalWorld) {
    use lash_durable::domain::RunFactKind;
    let nodes = world.nodes.lock_recover().upgrade().unwrap();
    loop {
        let slow: std::collections::BTreeSet<_> = world
            .entries()
            .into_iter()
            .filter(|(_, entries)| entries.iter().any(|(tool, _)| tool == SLOW))
            .map(|(call, _)| call)
            .collect();
        let trace = nodes.script().trace();
        let parked: std::collections::BTreeSet<_> = trace
            .iter()
            .filter(|write| write.committed())
            .flat_map(|write| &write.facts.rows)
            .filter(|row| row.kind == RunFactKind::XWait)
            .filter_map(|row| row.call.clone())
            .collect();
        if slow.len() == 2 && slow.is_subset(&parked) {
            return;
        }
        let settled = trace
            .iter()
            .filter(|write| write.stored != Stored::Pending)
            .count();
        nodes.script().settled(settled + 1).await;
    }
}

pub(super) async fn wait_plain_final(world: &ExternalWorld) {
    wait_terminal(world, lash_durable::domain::RunFactKind::XFinalOutcome).await;
}

pub(super) async fn wait_typed_cancel(world: &ExternalWorld) {
    wait_terminal(world, lash_durable::domain::RunFactKind::XOutcome).await;
}

async fn wait_terminal(world: &ExternalWorld, kind: lash_durable::domain::RunFactKind) {
    let nodes = world.nodes.lock_recover().upgrade().unwrap();
    loop {
        let selected = world.entries().into_iter().find_map(|(id, entries)| {
            entries
                .iter()
                .any(|(tool, args)| tool == SLOW && written_x(args) == Some(1))
                .then_some(id)
        });
        let trace = nodes.script().trace();
        if selected.is_some()
            && trace.iter().any(|write| {
                write.committed()
                    && write
                        .facts
                        .rows
                        .iter()
                        .any(|row| row.call == selected && row.kind == kind)
            })
        {
            return;
        }
        let settled = trace
            .iter()
            .filter(|write| write.stored != Stored::Pending)
            .count();
        nodes.script().settled(settled + 1).await;
    }
}

/// Exercise the public host target, including foreign/unknown and retry
/// requests. The durable rows and external log remain the decision oracle.
pub(super) async fn host_request(world: &ExternalWorld) {
    let (host, run) = world.cancel_host.lock_recover().clone().unwrap();
    let call = world
        .entries()
        .into_iter()
        .find_map(|(id, entries)| {
            entries
                .iter()
                .any(|(tool, args)| tool == SLOW && written_x(args) == Some(1))
                .then_some(id)
        })
        .unwrap();
    for (run, call) in [
        (run.clone(), call.child(987)),
        (
            lash_vocabulary::identity::RunId::from_turn_id(
                lash_vocabulary::identity::TurnId::try_from("foreign-run".to_owned()).unwrap(),
            ),
            call.clone(),
        ),
    ] {
        assert!(matches!(
            host.cancel(lash::CancelTarget::ToolCall { run, call })
                .await
                .unwrap(),
            lash::CancelReceipt::UnknownOrRevoked
        ));
    }
    let target = lash::CancelTarget::ToolCall { run, call };
    let invalid_mode = host
        .cancel(target.clone())
        .mode(lash::TurnCancelMode::Immediate)
        .await;
    let invalid_undelivered = host
        .cancel(target.clone())
        .undelivered(lash::TurnCancelUndeliveredInputPolicy::Defer)
        .await;
    for result in [invalid_mode, invalid_undelivered] {
        assert!(matches!(result, Err(lash::EmbedError::Runtime(error))
            if error.code == lash::runtime::RuntimeErrorCode::InvalidTurnCancelRequest));
    }
    let nodes = world.nodes.lock_recover().upgrade().unwrap();
    let lash::CancelTarget::ToolCall {
        run: target_run,
        call: target_call,
    } = &target
    else {
        unreachable!()
    };
    let request = lash_durable::domain::ToolCallCancelRequest {
        session: session(),
        run: target_run.turn_id().clone(),
        call: target_call.clone(),
        request_id: "host-call-cancel".to_owned(),
        origin: Some("host-race-loser".to_owned()),
        reason: Some("cancel the selected call".to_owned()),
    };
    loop {
        let mut tx = lash_durable::MailTx::new();
        tx.write(lash_durable::domain::MailDomainWrite::RequestToolCallCancel(request.clone()));
        let producer = nodes.producer("host");
        let next = 1 + nodes
            .script()
            .trace()
            .iter()
            .filter(|write| write.point.label == CommitLabel::CALL_CANCEL_REQUEST)
            .count();
        let recovery = async {
            let cut = nodes.script().first_cut().await;
            if cut.point.label != CommitLabel::CALL_CANCEL_REQUEST
                || cut.point.nth != next
                || !cut.fault.kills()
            {
                std::future::pending::<()>().await;
            }
            loop {
                let trace = nodes.script().trace();
                if trace
                    .iter()
                    .any(|write| write.point == cut.point && write.stored != Stored::Pending)
                {
                    break;
                }
                let settled = trace
                    .iter()
                    .filter(|write| write.stored != Stored::Pending)
                    .count();
                nodes.script().settled(settled + 1).await;
            }
        };
        tokio::select! {
            result = producer.commit_mail(tx, CommitLabel::CALL_CANCEL_REQUEST) => {
                if result.is_ok() { break; }
            }
            _ = recovery => {}
        }
    }
    let receipt = host
        .cancel(target.clone())
        .request_id("host-call-cancel")
        .origin("host-race-loser")
        .reason("cancel the selected call")
        .await
        .unwrap();
    let repeated = host
        .cancel(target)
        .request_id("do-not-replace-the-first")
        .origin("wrong-origin")
        .reason("wrong-reason")
        .await
        .unwrap();
    let case = world.cancel_case.lock_recover().unwrap();
    let expected_final = case.order.final_wins();
    for receipt in [&receipt, &repeated] {
        match receipt {
            lash::CancelReceipt::ToolCall {
                outcome: lash::ToolCallCancelOutcome::FinalWon,
                ..
            } if expected_final => {}
            lash::CancelReceipt::ToolCall {
                outcome: lash::ToolCallCancelOutcome::Cancelled,
                ..
            } if !expected_final => {}
            lash::CancelReceipt::ToolCall {
                outcome:
                    lash::ToolCallCancelOutcome::AlreadyRequested {
                        request_id,
                        origin,
                        reason,
                    },
                ..
            } if !expected_final => {
                assert_eq!(request_id, "host-call-cancel");
                assert_eq!(origin.as_deref(), Some("host-race-loser"));
                assert_eq!(reason.as_deref(), Some("cancel the selected call"));
            }
            other => panic!("the host receipt disagrees with the committed order: {other:?}"),
        }
    }
    if case.source == CancelSource::Host && case.explicit && !expected_final {
        loop {
            let trace = nodes.script().trace();
            if trace
                .iter()
                .any(|write| write.point.label == CommitLabel::CALL_CANCEL && write.committed())
            {
                break;
            }
            let settled = trace
                .iter()
                .filter(|write| write.stored != Stored::Pending)
                .count();
            nodes.script().settled(settled + 1).await;
        }
    }
}
