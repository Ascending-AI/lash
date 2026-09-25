use super::*;

/// One contract's generated-fact vocabulary, tied to the imported spec row
/// that owns its semantic oracle. The dispatcher keys rows on
/// `spec.semantic_oracle` and the proof-event check reads `spec.test_name`, so
/// a fact can only ever be built and attributed under the name its spec
/// carries — there is no second column of contract names to drift.
pub(super) struct ContractFactSpec {
    pub spec: &'static ScenarioContractSpec,
    pub fact: &'static str,
    pub assertion: &'static str,
    /// Reads the replayed contract-execution result and returns the
    /// contract-specific `observed` payload for the generated fact.
    pub check: fn(&Value, &'static str) -> Result<Value, String>,
    /// Facts the contract publishes before its contract-execution fact.
    pub extras_before: &'static [ExtraFact],
    /// Facts the contract publishes after its contract-execution fact.
    pub extras_after: &'static [ExtraFact],
}

/// A generated fact a contract publishes alongside its contract-execution
/// proof fact. Each variant names the helper and carries exactly the
/// arguments that helper needs.
pub(super) enum ExtraFact {
    ProviderMutation {
        mutation: &'static str,
        fact: &'static str,
        assertion: &'static str,
    },
    Exec {
        fact: &'static str,
        requirement: ExecFactRequirement,
    },
    ToolReentry {
        fact: &'static str,
        require_provider_event_release: bool,
    },
    DurableReplay(&'static str),
    ObserverReconnect(&'static str),
    BackendRetry(&'static str),
    TriggerThenProvider(&'static str),
    Custom(fn(&[DeliveredBoundary]) -> Result<ScenarioContractGeneratedFact, String>),
}

impl ExtraFact {
    fn build(&self, events: &[DeliveredBoundary]) -> Result<ScenarioContractGeneratedFact, String> {
        match *self {
            Self::ProviderMutation {
                mutation,
                fact,
                assertion,
            } => provider_mutation_semantic_fact(events, mutation, fact, assertion),
            Self::Exec { fact, requirement } => exec_semantic_fact(events, fact, requirement),
            Self::ToolReentry {
                fact,
                require_provider_event_release,
            } => tool_reentry_fact(events, fact, require_provider_event_release),
            Self::DurableReplay(fact) => durable_replay_fact(events, fact),
            Self::ObserverReconnect(fact) => observer_reconnect_fact(events, fact),
            Self::BackendRetry(fact) => backend_retry_terminalization_fact(events, fact),
            Self::TriggerThenProvider(fact) => trigger_then_provider_fact(events, fact),
            Self::Custom(build) => build(events),
        }
    }
}

/// Derives a contract-execution proof fact from a row's spec and check
/// function. Each family supplies its own because the shared result preamble
/// (`execution_api`, `driver`, ...) differs per suite.
pub(super) type ExecutionFactFn = fn(
    &[DeliveredBoundary],
    &'static ContractFactSpec,
    &ScenarioFactMemo,
) -> Result<ScenarioContractGeneratedFact, String>;

impl ContractFactSpec {
    /// The contract's full generated fact list: extras, then the
    /// contract-execution proof fact, in publication order.
    pub(super) fn generated_facts(
        &'static self,
        execution_fact: ExecutionFactFn,
        events: &[DeliveredBoundary],
        memo: &ScenarioFactMemo,
    ) -> Result<Vec<ScenarioContractGeneratedFact>, String> {
        let mut facts = Vec::with_capacity(self.extras_before.len() + 1 + self.extras_after.len());
        for extra in self.extras_before {
            facts.push(extra.build(events)?);
        }
        facts.push(execution_fact(events, self, memo)?);
        for extra in self.extras_after {
            facts.push(extra.build(events)?);
        }
        Ok(facts)
    }
}

/// `standard.max_turns_after_tool_result` anchors its generated facts on
/// generated tool/provider boundaries rather than a contract-execution proof
/// event, so it deliberately has no `ContractFactSpec` row. The dispatcher and
/// the registry exhaustiveness test name it through this constant instead of
/// repeating the literal.
pub(super) const NO_EXECUTION_FACT_CONTRACT: &ScenarioContractSpec = contract_spec(
    STANDARD_PROTOCOL_SCENARIO_CONTRACTS,
    "standard.max_turns_after_tool_result",
);

/// Compile-time lookup of an imported contract spec by semantic oracle. A
/// fact-spec row that names a contract the spec table does not carry fails
/// the build rather than publishing under a dangling name.
pub(super) const fn contract_spec(
    contracts: &'static [ScenarioContractSpec],
    semantic_oracle: &str,
) -> &'static ScenarioContractSpec {
    let mut index = 0;
    while index < contracts.len() {
        if str_eq(contracts[index].semantic_oracle, semantic_oracle) {
            return &contracts[index];
        }
        index += 1;
    }
    panic!("no imported scenario contract spec carries this semantic oracle")
}

const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut index = 0;
    while index < a.len() {
        if a[index] != b[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// Every fact-spec row across the contract families, in the order the
/// fixture contract list historically enumerated them (standard, rlm, agent).
#[cfg(test)]
pub(super) fn all_contract_fact_specs() -> impl Iterator<Item = &'static ContractFactSpec> {
    [
        STANDARD_CONTRACT_FACT_SPECS,
        RLM_CONTRACT_FACT_SPECS,
        AGENT_CONTRACT_FACT_SPECS,
    ]
    .into_iter()
    .flat_map(|rows| rows.iter())
}

/// The registry lookup the generated-facts dispatcher runs on: a semantic
/// oracle resolves to its row plus that row's family execution-fact builder.
pub(super) fn contract_fact_row(
    semantic_oracle: &str,
) -> Option<(&'static ContractFactSpec, ExecutionFactFn)> {
    [
        (
            STANDARD_CONTRACT_FACT_SPECS,
            standard_protocol_execution_fact as ExecutionFactFn,
        ),
        (
            RLM_CONTRACT_FACT_SPECS,
            rlm_protocol_execution_fact as ExecutionFactFn,
        ),
        (
            AGENT_CONTRACT_FACT_SPECS,
            agent_contract_execution_fact as ExecutionFactFn,
        ),
    ]
    .into_iter()
    .flat_map(|(rows, execution_fact)| rows.iter().map(move |row| (row, execution_fact)))
    .find(|(row, _)| row.spec.semantic_oracle == semantic_oracle)
}

pub(super) fn require_bool(
    result: &Value,
    pointer: &str,
    expected: bool,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_bool) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}={expected}"))
    }
}

pub(super) fn require_u64(
    result: &Value,
    pointer: &str,
    expected: u64,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_u64) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}={expected}"))
    }
}

pub(super) fn require_str(
    result: &Value,
    pointer: &str,
    expected: &str,
    contract: &str,
) -> Result<(), String> {
    if result.pointer(pointer).and_then(Value::as_str) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{contract} expected {pointer}=`{expected}`"))
    }
}

pub(super) fn require_checkpoint(
    result: &Value,
    checkpoint: &str,
    contract: &str,
) -> Result<(), String> {
    if result
        .get("checkpoints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.as_str() == Some(checkpoint))
    {
        Ok(())
    } else {
        Err(format!("{contract} missing checkpoint `{checkpoint}`"))
    }
}
