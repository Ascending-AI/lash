use super::*;

const REQUIRED_CASES: &[&str] = &[
    "queueing inputs",
    "cancellation",
    "observer reconnects",
    "malformed SSE",
    "rate limits",
    "tool results",
    "exec-code results",
    "durable effects",
    "refused commits",
    "lost replies",
    "backend retries",
    "durable redrives",
    "interleaving",
    "suspend/resume",
    "transport mutation",
    "different spines",
];

/// Bounded union of completed seeds' observations, shared by evidence and search.
/// No event or summary is retained; search memory does not grow with its fleet.
#[derive(Default)]
pub(crate) struct FleetCoverage {
    seen: BTreeSet<&'static str>,
    contracts: BTreeSet<String>,
    first_spine: Option<[usize; 8]>,
}

impl FleetCoverage {
    pub(crate) fn observe(
        &mut self,
        events: &[DeliveredBoundary],
        summary: &AbstractWorldView,
        verdicts: &[OracleVerdict],
    ) {
        for session in &summary.sessions {
            for (case, present) in [
                ("queueing inputs", session.queued_ingress_count > 0),
                ("cancellation", session.cancellation_count > 0),
                ("observer reconnects", session.observer_reconnects > 0),
                ("tool results", !session.tool_outputs.is_empty()),
                ("exec-code results", !session.exec_code_outputs.is_empty()),
            ] {
                if present {
                    self.seen.insert(case);
                }
            }
        }
        if summary
            .durable_effects
            .iter()
            .any(|effect| effect.execution_count == 1)
        {
            self.seen.insert("durable effects");
        }
        if durable_effect_replay_semantics(events, summary) {
            self.seen.insert("durable redrives");
        }
        for event in events {
            match event.kind {
                BoundaryKind::ProviderMutation => {
                    let mutation = event
                        .observed
                        .get("mutation")
                        .or_else(|| event.payload.get("mutation"))
                        .and_then(Value::as_str);
                    match mutation {
                        Some("malformed_sse_chunk") => {
                            self.seen.insert("malformed SSE");
                        }
                        Some("rate_limit_error_envelope") => {
                            self.seen.insert("rate limits");
                        }
                        Some(name) if is_transport_provider_mutation(name) => {
                            self.seen.insert("transport mutation");
                        }
                        _ => {}
                    }
                }
                BoundaryKind::BackendFailure => {
                    match event.payload.get("fault").and_then(Value::as_str) {
                        Some("refused") => {
                            self.seen.insert("refused commits");
                        }
                        Some("reply_lost") => {
                            self.seen.insert("lost replies");
                        }
                        _ => {}
                    }
                    if event
                        .observed
                        .get("attempt")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        > 1
                    {
                        self.seen.insert("backend retries");
                    }
                }
                _ => {}
            }
            if event.observed.get("runtime_suspend").is_some()
                || event.payload.get("suspend_resume").and_then(Value::as_bool) == Some(true)
            {
                self.seen.insert("suspend/resume");
            }
        }
        if peak_concurrent_live_turns(events) >= 2 {
            self.seen.insert("interleaving");
        }
        let spine = [
            BoundaryKind::Provider,
            BoundaryKind::QueuedIngress,
            BoundaryKind::Observer,
            BoundaryKind::BackendFailure,
            BoundaryKind::ProviderMutation,
            BoundaryKind::Tool,
            BoundaryKind::ExecCode,
            BoundaryKind::DurableEffect,
        ]
        .map(|kind| events.iter().filter(|event| event.kind == kind).count());
        match self.first_spine {
            None => self.first_spine = Some(spine),
            Some(first) if first != spine => {
                self.seen.insert("different spines");
            }
            _ => {}
        }
        for contract in all_scenario_contracts() {
            let id = scenario_contract_oracle_id(contract);
            if verdicts
                .iter()
                .any(|verdict| verdict.is_passed() && verdict.oracle_id == id)
            {
                self.contracts.insert(id);
            }
        }
    }

    pub(crate) fn verdict(&self) -> OracleVerdict {
        let mut missing = REQUIRED_CASES
            .iter()
            .filter(|case| !self.seen.contains(**case))
            .map(|case| (*case).to_string())
            .collect::<Vec<_>>();
        // Every conditional semantic proof must run somewhere in the fleet.
        // Passing a narrow seed cannot quietly remove a contract from certification.
        for contract in all_scenario_contracts() {
            let id = scenario_contract_oracle_id(contract);
            if !self.contracts.contains(&id) {
                missing.push(id);
            }
        }
        if missing.is_empty() {
            OracleVerdict::passed(
                FLEET_OPERATIONAL_COVERAGE_ORACLE,
                "completed swarm fleet covered operational cases, workload classes, different spines, and every scenario contract",
            )
        } else {
            OracleVerdict::failed(
                FLEET_OPERATIONAL_COVERAGE_ORACLE,
                format!("completed swarm fleet missed: {}", missing.join(", ")),
            )
        }
    }
}
