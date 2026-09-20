use super::*;

#[test]
fn restore_state_adopts_generation_at_or_above_three() {
    // Cold rebuild ratchet: a session whose tool catalog advanced to
    // generation >= 3 restores onto a fresh base-1 registry. `restore_state`
    // adopts the snapshot's generation verbatim; `apply_state` (a gen-matched
    // delta) rejects it. This is the exact divergence the durable worker /
    // session resume rebuild relies on `restore_state` to absorb.
    let source = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("source registry");
    let snapshot = source.export_state().with_generation_for_conformance(3);

    let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target registry");
    assert_eq!(
        target.generation(),
        1,
        "a fresh registry starts at generation 1"
    );
    let restored = target
        .restore_state(snapshot.clone())
        .expect("restore adopts the snapshot generation");
    assert_eq!(
        restored.generation, 3,
        "restore returns the adopted generation"
    );
    assert!(restored.is_clean(), "all tools resolve, so nothing orphans");
    assert_eq!(
        target.generation(),
        3,
        "restore adopts gen 3 onto a base-1 registry without bumping"
    );
    // A re-export round-trips at the same generation (idempotent).
    assert_eq!(target.export_state().generation(), 3);

    // apply_state on the same high-generation snapshot is rejected — proving
    // the rebuild would have failed without restore_state.
    let fresh = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("fresh registry");
    assert!(
        matches!(
            fresh.apply_state(snapshot),
            Err(ReconfigureError::GenerationMismatch {
                expected: 3,
                actual: 1
            })
        ),
        "apply_state must reject a gen-3 snapshot on a base-1 registry"
    );
}

/// Build a snapshot whose `mcp__demo__search` entry only resolves while
/// `ExternalMockSource` is registered — restoring it elsewhere orphans it.
fn snapshot_with_external_tool() -> ToolState {
    let source = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("source registry");
    source
        .upsert_source(Arc::new(ExternalMockSource))
        .expect("source registered");
    source.export_state()
}

#[tokio::test]
async fn restore_orphans_unresolved_tools_instead_of_failing() {
    let snapshot = snapshot_with_external_tool();

    let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
    let report = target
        .restore_state(snapshot)
        .expect("restore tolerates the missing source");
    assert_eq!(report.lost_members, vec![tool_id("mcp__demo__search")]);
    assert!(report.parked_opt_outs.is_empty());
    assert!(report.superseded_identities.is_empty());

    // Orphans are non-members: excluded from the catalog listing entirely.
    assert!(
        !target
            .tool_manifests()
            .into_iter()
            .any(|manifest| manifest.name == "mcp__demo__search"),
        "orphans are excluded from the catalog"
    );
    let exported = target.export_state();
    assert!(
        !exported
            .tool_manifests()
            .into_iter()
            .any(|manifest| manifest.name == "mcp__demo__search"),
        "exported ToolState also excludes the orphan from the catalog"
    );
    let entry = exported
        .get(&tool_id("mcp__demo__search"))
        .expect("orphan exported");
    assert!(entry.is_orphaned());
    assert!(!entry.is_member(), "orphans are never catalog members");

    // Execution fails loudly with a precise error.
    let context = test_attempt_context();
    let args = json!({ "query": "hello" });
    let result = execute_leaf_by_id(&target, &tool_id("mcp__demo__search"), &args, &context).await;
    assert!(!result.is_success());
    assert!(
        format!("{result:?}").contains("unavailable"),
        "orphan execution error names the condition: {result:?}"
    );

    // Bound tools are unaffected.
    assert!(target.resolve_contract("mock_tool").is_some());
}

#[tokio::test]
async fn crafted_orchestrating_orphan_cannot_block_a_legitimate_leaf_registration() {
    let source = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("source registry");
    let mut crafted_blob =
        serde_json::to_value(source.export_state()).expect("serialize leaf state");
    crafted_blob["tools"]["tool:mock_tool"]["registration_kind"] = json!("orchestrating");
    let crafted_snapshot: ToolState =
        serde_json::from_value(crafted_blob).expect("deserialize crafted state");

    let target = ToolRegistry::empty();
    let report = target
        .restore_state(crafted_snapshot)
        .expect("an unresolved crafted entry remains an orphan");
    assert_eq!(report.lost_members, vec![tool_id("mock_tool")]);
    assert!(target.is_orchestrating_tool(&tool_id("mock_tool")));

    let orphan_result = execute_leaf_by_id(
        &target,
        &tool_id("mock_tool"),
        &json!({}),
        &test_attempt_context(),
    )
    .await;
    assert!(
        !orphan_result.is_success(),
        "a claimed lane never makes an orphan executable"
    );
    assert!(format!("{orphan_result:?}").contains("unavailable"));

    target
        .upsert_source(Arc::new(ToolProviderSource::new(
            "legitimate-leaf",
            vec![Arc::new(MockTool)],
        )))
        .expect("the live leaf lane supersedes the stored claim");
    assert!(
        !target.is_orchestrating_tool(&tool_id("mock_tool")),
        "the rebound kind comes from the legitimate live source"
    );
    let rebound = execute_leaf_by_id(
        &target,
        &tool_id("mock_tool"),
        &json!({}),
        &test_attempt_context(),
    )
    .await;
    assert!(rebound.is_success(), "the legitimate leaf executes");
    assert_eq!(rebound.value_for_projection(), json!("ok"));
}

#[tokio::test]
async fn orphan_rebinds_when_source_is_upserted_again() {
    let snapshot = snapshot_with_external_tool();
    let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
    target.restore_state(snapshot).expect("restore");
    let orphaned_generation = target.generation();

    target
        .upsert_source(Arc::new(ExternalMockSource))
        .expect("the returning source must not conflict with its own orphan");
    assert!(
        target.generation() > orphaned_generation,
        "rebinding bumps the generation"
    );

    let exported = target.export_state();
    let entry = exported
        .get(&tool_id("mcp__demo__search"))
        .expect("entry kept");
    assert!(
        !entry.is_orphaned(),
        "the orphan rebound to the live source"
    );
    assert!(
        entry.is_member(),
        "the rebound tool is a catalog member again"
    );

    let context = test_attempt_context();
    let args = json!({ "query": "hello" });
    let result = execute_leaf_by_id(&target, &tool_id("mcp__demo__search"), &args, &context).await;
    assert!(result.is_success(), "rebound tool executes: {result:?}");
}

#[test]
fn restore_uses_live_manifest_and_preserves_membership_for_same_id() {
    struct UpdatedMockTool;

    #[async_trait::async_trait]
    impl ToolProvider for UpdatedMockTool {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            manifests(vec![test_tool("mock_tool", "live manifest")])
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            contract_from(vec![test_tool("mock_tool", "live manifest")], name)
        }

        async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(json!("updated")).into()
        }
    }

    let source = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("source");
    let mut snapshot = source.export_state();
    snapshot
        .set_membership(&tool_id("mock_tool"), false)
        .expect("opt out");
    let target =
        ToolRegistry::from_tool_provider(Arc::new(UpdatedMockTool)).expect("target registry");

    target.restore_state(snapshot).expect("restore");
    let exported = target.export_state();
    let entry = exported.get(&tool_id("mock_tool")).expect("same id");
    assert_eq!(entry.manifest().description, "live manifest");
    assert!(!entry.is_member(), "membership remains attached to the id");
}

#[test]
fn orphan_rebinds_at_explicit_source_admission() {
    let snapshot = host_only_snapshot(1);
    let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
    let report = target.restore_state(snapshot).expect("restore");
    assert_eq!(report.lost_members, vec![tool_id("host_only")]);

    target
        .upsert_source(Arc::new(NamedExactSource { id: "exact-a" }))
        .expect("source admission re-derives the surface");
    let manifest = target
        .resolve_manifest("host_only")
        .expect("admitted source rebound the persisted id");
    assert_eq!(manifest.name, "host_only");
    let entry = target.export_state();
    let entry = entry.get(&tool_id("host_only")).expect("entry kept");
    assert!(
        !entry.is_orphaned(),
        "source admission clears the orphan flag"
    );
    assert!(entry.is_member(), "the rebound tool is a catalog member");
}

#[test]
fn restore_binds_snapshot_id_from_source_that_advertises_nothing() {
    let snapshot = host_only_snapshot(1);

    let target = ToolRegistry::empty();
    target
        .upsert_source(Arc::new(NamedExactSource { id: "exact-a" }))
        .expect("lazy source registered before restore");
    let report = target.restore_state(snapshot).expect("lazy id binds");

    assert!(report.is_clean());
    let exported = target.export_state();
    let entry = exported
        .get(&tool_id("host_only"))
        .expect("snapshot-only id retained");
    assert!(!entry.is_orphaned());
    assert!(entry.is_member());
}

#[tokio::test]
async fn source_admission_preserves_snapshot_curation_without_authority_latching() {
    let target = ToolRegistry::empty();
    target
        .upsert_source(Arc::new(NamedExactSource { id: "exact-a" }))
        .expect("exact source registered");
    target
        .restore_state(host_only_snapshot(1))
        .expect("snapshot id admitted from the exact source");

    let result = execute_leaf_by_id(
        &target,
        &tool_id("host_only"),
        &json!({}),
        &test_attempt_context(),
    )
    .await;

    assert!(
        result.is_success(),
        "authority policy is not registry curation: {result:?}"
    );
    assert!(
        target
            .export_state()
            .get(&tool_id("host_only"))
            .expect("admitted tool recorded")
            .is_member()
    );
}

#[test]
fn restore_drops_superseded_orphan_and_does_not_transfer_opt_out() {
    struct ReplacedSearchTool;
    #[async_trait::async_trait]
    impl ToolProvider for ReplacedSearchTool {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            manifests(vec![ToolDefinition::raw(
                "tool:replaced",
                "mcp__demo__search",
                "a different implementation under the same name",
                ToolDefinition::default_input_schema(),
                json!({}),
            )])
        }
        fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
            None
        }
        async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(json!("ok")).into()
        }
    }

    let mut snapshot = snapshot_with_external_tool();
    snapshot
        .set_membership(&tool_id("mcp__demo__search"), false)
        .expect("opt out old id");
    let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
    target
        .add_tool_provider(Arc::new(ReplacedSearchTool))
        .expect("replacement registered");
    let report = target
        .restore_state(snapshot)
        .expect("same name with a different id supersedes the old orphan");
    // A replaced identity is neither loss nor an opt-out: the capability is
    // live under a new id, and the report says so by name (FIG-3367).
    assert!(report.lost_members.is_empty());
    assert!(report.parked_opt_outs.is_empty());
    assert_eq!(
        report.superseded_identities,
        vec![crate::tool_registry::SupersededToolIdentity {
            retired_id: tool_id("mcp__demo__search"),
            live_id: crate::ToolId::from("tool:replaced"),
            name: "mcp__demo__search".to_string(),
        }]
    );

    let exported = target.export_state();
    assert!(
        !exported.contains(&tool_id("mcp__demo__search")),
        "the old unresolved grant is superseded by the live name"
    );
    assert!(
        exported
            .get(&crate::ToolId::from("tool:replaced"))
            .is_some_and(ToolStateEntry::is_member),
        "membership policy is per id, so the replacement defaults to member"
    );
}

#[test]
fn apply_state_round_trips_while_orphans_exist() {
    // `export_state` → edit → `apply_state` must work with an orphan in
    // the snapshot: the exported orphan flag exempts it from strictness.
    let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
    target
        .restore_state(snapshot_with_external_tool())
        .expect("restore");

    let mut edited = target.export_state();
    edited
        .set_membership(&tool_id("mock_tool"), false)
        .expect("edit bound tool");
    target
        .apply_state(edited)
        .expect("apply accepts the snapshot it exported");
    let exported = target.export_state();
    assert!(
        exported
            .get(&tool_id("mcp__demo__search"))
            .unwrap()
            .is_orphaned()
    );
    assert!(
        !exported.get(&tool_id("mock_tool")).unwrap().is_member(),
        "the host-removed bound tool stays a non-member through the round-trip"
    );

    // But a snapshot that does NOT mark the tool orphaned still fails —
    // strictness is preserved for entries that were bound at export.
    let strict = snapshot_with_external_tool().with_generation_for_conformance(target.generation());
    assert!(matches!(
        target.apply_state(strict),
        Err(ReconfigureError::Validation(_))
    ));
}

#[test]
fn orphan_flag_serializes_on_every_entry_and_is_required() {
    let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
    target
        .restore_state(snapshot_with_external_tool())
        .expect("restore");
    let value = serde_json::to_value(target.export_state()).expect("serializes");
    assert_eq!(
        value["tools"]["tool:mcp__demo__search"]["orphaned"],
        json!(true)
    );
    assert_eq!(
        value["tools"]["tool:mock_tool"]["orphaned"],
        json!(false),
        "every entry states the flag: a snapshot its writer cannot decode is useless"
    );

    let error = serde_json::from_value::<ToolStateEntry>(json!({
        "manifest": value["tools"]["tool:mock_tool"]["manifest"],
        "registration_kind": "leaf"
    }))
    .expect_err("a pre-cutover entry without the flag must be refused");
    assert!(
        error.to_string().contains("orphaned"),
        "the refusal must name the missing field: {error}"
    );
}

#[test]
fn member_false_decodes_as_host_curation_intent() {
    let source = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("source");
    let manifest = serde_json::to_value(
        source
            .export_state()
            .get(&tool_id("mock_tool"))
            .expect("mock entry")
            .manifest(),
    )
    .expect("serialize mock manifest");
    let entry: ToolStateEntry = serde_json::from_value(json!({
        "manifest": manifest,
        "orphaned": false,
        "member": false,
        "registration_kind": "leaf"
    }))
    .expect("non-member entry decodes");

    let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
    target
        .restore_state(ToolState::new(
            1,
            [(tool_id("mock_tool"), entry)].into_iter().collect(),
        ))
        .expect("non-member curation restores against the live source");

    assert!(
        !target
            .export_state()
            .get(&tool_id("mock_tool"))
            .expect("restored mock entry")
            .is_member(),
        "legacy member=false remains an explicit host opt-out"
    );
}

#[test]
fn remove_source_removes_all_source_tools() {
    let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
    registry
        .upsert_source(Arc::new(ExternalMockSource))
        .expect("source registered");
    registry
        .remove_source_id("external")
        .expect("source removed");
    let defs = registry.tool_manifests();
    assert!(!defs.iter().any(|def| def.name == "mcp__demo__search"));
}

#[test]
fn remove_source_preserves_non_member_curation_across_reattach() {
    let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
    registry
        .upsert_source(Arc::new(ExternalMockSource))
        .expect("source registered");
    let external_id = tool_id("mcp__demo__search");
    let mut disabled = registry.export_state();
    disabled
        .set_membership(&external_id, false)
        .expect("external tool exists");
    registry
        .apply_state(disabled)
        .expect("disable external tool");

    let before_detach = registry.export_state();
    let disabled_entry = before_detach
        .get(&external_id)
        .expect("precondition: external tool remains stored while disabled");
    assert!(
        !disabled_entry.member && !disabled_entry.is_orphaned(),
        "precondition: the live external tool stores an explicit member=false opt-out"
    );

    registry
        .remove_source_id("external")
        .expect("source detached");
    let detached = registry.export_state();
    let detached_entry = detached
        .get(&external_id)
        .expect("detaching a source keeps its tools as orphans");
    assert!(detached_entry.is_orphaned());
    assert!(
        !detached_entry.member,
        "the orphan retains the stored member=false curation bit"
    );

    let encoded = serde_json::to_value(&detached).expect("serialize detached state");
    let decoded: ToolState = serde_json::from_value(encoded).expect("deserialize detached state");
    let restored = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("restore registry");
    let report = restored
        .restore_state(decoded)
        .expect("restore detached state");
    // The stored `member: false` curation makes this a parked opt-out, not a
    // lost capability: nothing the session could have used went missing.
    assert!(report.lost_members.is_empty());
    assert_eq!(report.parked_opt_outs, vec![external_id.clone()]);
    assert!(
        restored
            .export_state()
            .get(&external_id)
            .is_some_and(|entry| entry.is_orphaned() && !entry.member),
        "the exported orphan round-trips with its curation bit"
    );

    registry
        .upsert_source(Arc::new(ExternalMockSource))
        .expect("source reattached");
    let rebound = registry.export_state();
    let rebound_entry = rebound.get(&external_id).expect("tool rebounds by id");
    assert!(!rebound_entry.is_orphaned());
    assert!(
        !rebound_entry.is_member(),
        "the rebound tool remains a non-member after detach and reattach"
    );
}

#[test]
fn project_tool_catalog_projects_all_members_with_catalog_metadata() {
    fn member_fixture(name: &str) -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            format!("desc for {name}"),
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({}),
        )
    }
    let catalog = project_tool_catalog(["read_file", "search_tools"].map(|name| {
        let definition = member_fixture(name);
        crate::ToolCatalogEntry {
            manifest: definition.manifest,
            contract: Arc::new(definition.contract),
        }
    }));
    assert_eq!(catalog.len(), 2);
    assert_eq!(catalog[0]["name"], serde_json::json!("read_file"));
    assert_eq!(
        catalog[0]["contract"]["signature"],
        serde_json::json!("read_file({})")
    );
    // Membership is the execution gate; the projection emits no tier.
    assert!(catalog[0].get("availability").is_none());
    assert!(catalog[0].get("showcased").is_none());
    assert!(catalog[0].get("callable").is_none());
    assert!(catalog[0].get("searchable").is_none());
    assert_eq!(catalog[1]["name"], serde_json::json!("search_tools"));
}

#[test]
fn project_tool_catalog_preserves_dynamic_output_contracts() {
    fn member_fixture(name: &str) -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            format!("desc for {name}"),
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({}),
        )
    }
    let definition = member_fixture("llm_query")
        .with_output_from_input_schema("output", Some(serde_json::json!({ "type": "string" })));
    let catalog = project_tool_catalog([crate::ToolCatalogEntry {
        manifest: definition.manifest,
        contract: Arc::new(definition.contract),
    }]);

    assert_eq!(
        catalog[0]["contract"]["signature"],
        serde_json::json!("llm_query<T = str>({})")
    );
    assert_eq!(catalog[0]["contract"]["returns"], serde_json::json!("T"));
}

struct RestoreProbeInternal {
    executed: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::InternalProcessToolImplementation for RestoreProbeInternal {
    async fn execute(&self, _call: crate::InternalProcessToolCall<'_>) -> crate::ToolOutcomeDone {
        self.executed.fetch_add(1, Ordering::SeqCst);
        crate::ToolOutcomeDone::ok(json!("internal-restored"))
    }
}

fn stored_internal_probe_entry() -> ToolStateEntry {
    ToolStateEntry::new(
        test_tool("internal_probe", "stored internal probe")
            .with_activation(crate::ToolActivation::Internal)
            .manifest(),
    )
}

/// A snapshot that recorded an internally activated tool rebinds to a matching
/// explicit internal source on the same id and executes through the internal
/// route.
#[tokio::test]
async fn restore_rebinds_stored_internal_entry_to_explicit_internal_source() {
    let executed = Arc::new(AtomicUsize::new(0));
    let registry = ToolRegistry::from_internal_tools(vec![crate::InternalProcessToolDef::new(
        test_tool("internal_probe", "stored internal probe"),
        Arc::new(RestoreProbeInternal {
            executed: Arc::clone(&executed),
        }),
    )])
    .expect("internal-only registry");
    let mut entries = BTreeMap::new();
    entries.insert(tool_id("internal_probe"), stored_internal_probe_entry());
    registry
        .restore_state(ToolState::new(registry.generation(), entries))
        .expect("a matching internal source restores the stored internal entry");

    let tool = test_tool_context();
    let context = crate::InternalProcessContext::__for_testing(&tool);
    let manifest = registry
        .resolve_manifest_by_id(&tool_id("internal_probe"))
        .expect("internal manifest resolves");
    let result = registry
        .execute_internal_process_tool(crate::InternalProcessToolCall::new(
            &manifest,
            &json!({}),
            &context,
        ))
        .await
        .expect("internal execution succeeds");
    assert_eq!(
        result.into_output().value_for_projection(),
        json!("internal-restored")
    );
    assert_eq!(executed.load(Ordering::SeqCst), 1);
}

/// A stored internal entry with no matching internal source stays orphaned:
/// unavailable rather than silently bound elsewhere.
#[tokio::test]
async fn restore_orphans_stored_internal_entry_without_a_matching_internal_source() {
    let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
    let mut entries = BTreeMap::new();
    entries.insert(tool_id("internal_probe"), stored_internal_probe_entry());
    let report = registry
        .restore_state(ToolState::new(registry.generation(), entries))
        .expect("an absent internal source orphans the stored entry");
    assert_eq!(report.lost_members, vec![tool_id("internal_probe")]);
    let entry = registry
        .export_state()
        .get(&tool_id("internal_probe"))
        .expect("orphan exported")
        .clone();
    assert!(entry.is_orphaned());
    assert!(
        !entry.is_member(),
        "an orphaned internal entry is unavailable"
    );
    assert!(
        !registry
            .tool_manifests()
            .into_iter()
            .any(|manifest| manifest.name == "internal_probe"),
        "the orphan is excluded from the catalog"
    );
}

/// A stored internal entry must never silently rebind to a live leaf source on
/// the same id; the restore errors before any execution.
#[tokio::test]
async fn restore_refuses_stored_internal_entry_claimed_by_a_live_leaf_source() {
    struct SameIdLeaf;

    #[async_trait::async_trait]
    impl ToolProvider for SameIdLeaf {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            manifests(vec![test_tool(
                "internal_probe",
                "live leaf on the same id",
            )])
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            contract_from(
                vec![test_tool("internal_probe", "live leaf on the same id")],
                name,
            )
        }

        async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(json!("leaf-ran")).into()
        }
    }

    let registry = ToolRegistry::from_tool_provider(Arc::new(SameIdLeaf)).expect("leaf registry");
    let mut entries = BTreeMap::new();
    entries.insert(tool_id("internal_probe"), stored_internal_probe_entry());
    let error = registry
        .restore_state(ToolState::new(registry.generation(), entries))
        .expect_err("a live leaf source on the same id cannot adopt a stored internal entry");
    let message = error.to_string();
    assert!(
        message.contains("tool:internal_probe") && message.contains("internal"),
        "the refusal names the id and the class mismatch: {message}"
    );
}
