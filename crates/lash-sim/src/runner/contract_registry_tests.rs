use super::*;

#[test]
fn fixed_contract_row_registry_matches_scenario_contract_specs() {
    let specs: Vec<&'static ScenarioContractSpec> = [
        STANDARD_PROTOCOL_SCENARIO_CONTRACTS,
        RLM_PROTOCOL_SCENARIO_CONTRACTS,
        AGENT_SCENARIO_CONTRACTS,
    ]
    .into_iter()
    .flat_map(|contracts| contracts.iter())
    .collect();
    let spec_ids: BTreeSet<&'static str> = specs.iter().map(|spec| spec.semantic_oracle).collect();
    fn row_pairs<'a>(
        rows: &'a [FixedContractRow<TurnMachineContractExecutor>],
    ) -> impl Iterator<Item = (&'static str, &'static str)> + 'a {
        rows.iter()
            .map(|row| (row.semantic_oracle, row.source_scenario))
    }
    let registry_ids: BTreeSet<&'static str> = row_pairs(STANDARD_CONTRACT_ROWS)
        .chain(row_pairs(RLM_CONTRACT_ROWS))
        .chain(
            AGENT_CONTRACT_ROWS
                .iter()
                .map(|row| (row.semantic_oracle, row.source_scenario)),
        )
        .map(|(semantic_oracle, _)| semantic_oracle)
        .collect();
    assert_eq!(
        registry_ids, spec_ids,
        "fixed contract rows must register exactly the declared semantic oracles",
    );

    for (semantic_oracle, source_scenario) in row_pairs(STANDARD_CONTRACT_ROWS)
        .chain(row_pairs(RLM_CONTRACT_ROWS))
        .chain(
            AGENT_CONTRACT_ROWS
                .iter()
                .map(|row| (row.semantic_oracle, row.source_scenario)),
        )
    {
        let spec = specs
            .iter()
            .find(|spec| spec.semantic_oracle == semantic_oracle)
            .unwrap_or_else(|| panic!("row `{semantic_oracle}` has no scenario spec"));
        assert_eq!(
            source_scenario, spec.test_name,
            "row `{semantic_oracle}` source scenario must name the spec's protocol test",
        );
    }
}
