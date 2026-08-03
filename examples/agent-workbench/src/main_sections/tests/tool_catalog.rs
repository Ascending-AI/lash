async fn assert_tool_catalog_contract(core: &lash::LashCore, session: &lash::LashSession) {
    let core_catalog: lash::ToolCatalogView = core.tool_catalog();
    let core_manifests = core_catalog.manifests();
    assert!(
        core_manifests
            .iter()
            .any(|manifest| manifest.name == "inbox__test__send"),
        "core catalog composes the workbench plugin's inbox tool"
    );
    let send_manifest = core_manifests
        .iter()
        .find(|manifest| manifest.name == "inbox__test__send")
        .expect("core catalog includes inbox.test send")
        .clone();
    let core_contract = core_catalog
        .resolve_contract("inbox__test__send")
        .expect("core catalog resolves inbox.test send");
    assert_eq!(
        core_contract.input_schema.canonical()["required"],
        serde_json::json!(["title"]),
        "core catalog exposes the runtime input schema"
    );
    assert!(
        matches!(
            core_contract.output_contract,
            lash::tools::ToolOutputContract::Static
        ),
        "core catalog exposes the runtime output contract"
    );
    let miss: lash::ToolCatalogMiss = core_catalog
        .resolve_contract("inbox__test__missing")
        .expect_err("unknown core tool has a typed miss");
    assert_eq!(miss.name, "inbox__test__missing");

    let session_tools = session.tools();
    let session_contract = session_tools
        .resolve_contract("inbox__test__send")
        .await
        .expect("session catalog resolves an active tool");
    assert_eq!(
        serde_json::to_value(session_contract.as_ref()).expect("serialize session contract"),
        serde_json::to_value(core_contract.as_ref()).expect("serialize core contract"),
        "core and initial session projections agree"
    );

    session_tools
        .set_membership(send_manifest.id.clone(), false)
        .await
        .expect("remove send from this session catalog");
    let gated = session_tools
        .resolve_contract("inbox__test__send")
        .await
        .expect_err("non-member tool must resolve as a typed miss in this session");
    assert_eq!(gated.name, "inbox__test__send");
    assert!(
        core_catalog.resolve_contract("inbox__test__send").is_ok(),
        "session catalog curation must not mutate the core projection"
    );
    session_tools
        .set_membership(send_manifest.id, true)
        .await
        .expect("restore send to this session catalog");
}
