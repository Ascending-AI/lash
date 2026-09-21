use super::*;

/// `await tool.read_file({ path: "." })?`
fn read_current_directory() -> Expr {
    builders::unwrap(builders::await_expr(builders::receiver_call(
        builders::var("tool"),
        "read_file",
        vec![builders::record(vec![("path", builders::string("."))])],
    )))
}

/// `process scan(tool: Tools) { finish (await tool.<operation>({ <field>: "." }))? }`
fn tool_call_process(operation: &str, field: &str) -> Program {
    builders::module(
        vec![builders::process(
            "scan",
            vec![builders::param("tool", TypeExpr::Ref("Tools".into()))],
            builders::block(vec![builders::finish(builders::unwrap(
                builders::await_expr(builders::receiver_call(
                    builders::var("tool"),
                    operation,
                    vec![builders::record(vec![(field, builders::string("."))])],
                )),
            ))]),
        )],
        Vec::new(),
    )
}

#[test]
fn label_annotations_require_enabled_language_feature() {
    // @label(title: "Scan files")
    // process scan(tool: Tools) {
    //   @label(title: "Read file")
    //   text = await tool.read_file({ path: "." })?
    //   finish text
    // }
    let program = builders::module(
        vec![builders::labelled_process(
            "scan",
            vec![builders::param("tool", TypeExpr::Ref("Tools".into()))],
            builders::label("Scan files", None),
            builders::block(vec![
                builders::labelled(
                    builders::label("Read file", None),
                    builders::assign("text", read_current_directory()),
                ),
                builders::finish(builders::var("text")),
            ]),
        )],
        Vec::new(),
    );

    let err = LinkedModule::link(program.clone(), full_host_environment())
        .expect_err("default surface should reject label annotations");
    assert!(matches!(
        err,
        LinkError::FeatureDisabled {
            feature: "label annotations",
            ..
        }
    ));

    let linked =
        LinkedModule::link(program, full_label_environment()).expect("enabled surface should link");
    assert!(
        linked
            .artifact
            .host_requirements
            .language_features
            .label_annotations
    );
    let process = linked.program().process("scan").expect("linked process");
    assert_eq!(
        process.label.as_ref().map(|label| label.title.as_str()),
        Some("Scan files")
    );
}

#[test]
fn disabled_label_annotation_in_main_reports_the_annotation_span() {
    // The diagnostic renders against a caller-supplied span table: TypeScript
    // programs carry lashlang spans only when the lowering populates them
    // (FIG-3065), so the test pins the two statement spans itself.
    let source = "count = 1\n@label(title: \"Finish up\") finish count\n";
    let program = builders::with_source_spans(
        builders::with_expression_spans(
            builders::program(vec![
                builders::assign("count", builders::num(1.0)),
                builders::labelled(
                    builders::label("Finish up", None),
                    builders::finish(builders::var("count")),
                ),
            ]),
            &[(0, 9), (10, 49)],
        ),
        &[(&[1], 10, 49)],
    );
    let err = LinkedModule::link(program, full_host_environment())
        .expect_err("default surface should reject label annotations");

    let LinkError::FeatureDisabled {
        feature: "label annotations",
        span,
    } = err
    else {
        panic!("unexpected link error: {err:?}");
    };
    let span = span.expect("annotated statement span");
    assert!(
        source[span.start..span.end].starts_with("@label(title: \"Finish up\")"),
        "reported span covers `{}`",
        &source[span.start..span.end]
    );
}

#[test]
fn label_metadata_round_trips_and_changes_artifact_identity() {
    // @label(title: "Scan files")
    // process scan(tool: Tools) {
    //   @label(title: <read title>, description: "Load source text")
    //   text = await tool.read_file({ path: "." })?
    //   @label(title: "Finish")
    //   finish text
    // }
    let annotated = |read_title: &str| {
        builders::module(
            vec![builders::labelled_process(
                "scan",
                vec![builders::param("tool", TypeExpr::Ref("Tools".into()))],
                builders::label("Scan files", None),
                builders::block(vec![
                    builders::labelled(
                        builders::label(read_title, Some("Load source text")),
                        builders::assign("text", read_current_directory()),
                    ),
                    builders::labelled(
                        builders::label("Finish", None),
                        builders::finish(builders::var("text")),
                    ),
                ]),
            )],
            Vec::new(),
        )
    };
    let first =
        LinkedModule::link(annotated("Read file"), full_label_environment()).expect("link first");
    let changed = LinkedModule::link(annotated("Read source"), full_label_environment())
        .expect("link changed");

    let bytes = first
        .artifact
        .to_store_bytes()
        .expect("encode annotated artifact");
    let decoded = ModuleArtifact::from_store_bytes(&bytes).expect("decode annotated artifact");
    assert_eq!(decoded, first.artifact);
    assert_ne!(first.module_ref, changed.module_ref);
    assert_ne!(
        first.artifact.process_ref("scan"),
        changed.artifact.process_ref("scan")
    );
}

#[test]
fn module_ref_ignores_spans_and_formatting() {
    // process scan(root: str) { finish root }
    //
    // The same declaration written out over several lines carries different
    // spans; identity must ignore them.
    let scan_process = || {
        builders::module(
            vec![builders::process(
                "scan",
                vec![builders::param("root", TypeExpr::Str)],
                builders::block(vec![builders::finish(builders::var("root"))]),
            )],
            Vec::new(),
        )
    };
    let compact = LinkedModule::link(
        builders::with_source_spans(scan_process(), &[(&[0], 0, 38)]),
        full_host_environment(),
    )
    .expect("link compact");
    let formatted = LinkedModule::link(
        builders::with_source_spans(scan_process(), &[(&[0], 17, 89)]),
        full_host_environment(),
    )
    .expect("link formatted");

    assert_eq!(compact.module_ref, formatted.module_ref);
}

#[test]
fn process_ref_tracks_abi_and_body_but_not_local_binder_names() {
    // process scan(<param>: str) { <binder> = <param>
    // finish <finish> }
    let scan_process = |param: &str, binder: &str, finish: Expr| {
        builders::module(
            vec![builders::process(
                "scan",
                vec![builders::param(param, TypeExpr::Str)],
                builders::block(vec![
                    builders::assign(binder, builders::var(param)),
                    builders::finish(finish),
                ]),
            )],
            Vec::new(),
        )
    };
    // process scan(root: str) { value = root
    // finish value }
    let original = LinkedModule::link(
        scan_process("root", "value", builders::var("value")),
        full_host_environment(),
    )
    .expect("link original");
    // process scan(root: str) { renamed = root
    // finish renamed }
    let renamed_local = LinkedModule::link(
        scan_process("root", "renamed", builders::var("renamed")),
        full_host_environment(),
    )
    .expect("link renamed local");
    // process scan(path: str) { value = path
    // finish value }
    let renamed_param = LinkedModule::link(
        scan_process("path", "value", builders::var("value")),
        full_host_environment(),
    )
    .expect("link renamed param");
    // process scan(root: str) { value = root
    // finish { value: value } }
    let changed_body = LinkedModule::link(
        scan_process(
            "root",
            "value",
            builders::record(vec![("value", builders::var("value"))]),
        ),
        full_host_environment(),
    )
    .expect("link changed body");

    assert_eq!(
        original.artifact.process_ref("scan"),
        renamed_local.artifact.process_ref("scan")
    );
    assert_ne!(
        original.artifact.process_ref("scan"),
        renamed_param.artifact.process_ref("scan")
    );
    assert_ne!(
        original.artifact.process_ref("scan"),
        changed_body.artifact.process_ref("scan")
    );
}

#[test]
fn host_requirements_ref_tracks_resource_requirements_not_unrelated_tools() {
    let mut with_extra = resources();
    with_extra
        .add_module_operation(
            ["tools"],
            "Tools",
            "unrelated",
            "unrelated",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    // process scan(tool: Tools) { finish (await tool.read_file({ path: "." }))? }
    let program = tool_call_process("read_file", "path");

    let base = LinkedModule::link(program.clone(), full_host_environment()).expect("link base");
    let extra = LinkedModule::link(
        program.clone(),
        LashlangHostEnvironment::new(with_extra, LashlangAbilities::all()),
    )
    .expect("link extra");
    // process scan(tool: Tools) { finish (await tool.echo({ value: "." }))? }
    let changed_requirement =
        LinkedModule::link(tool_call_process("echo", "value"), full_host_environment())
            .expect("link changed requirement");

    assert_eq!(base.module_ref, extra.module_ref);
    assert_eq!(base.host_requirements_ref, extra.host_requirements_ref);
    assert_ne!(
        base.host_requirements_ref,
        changed_requirement.host_requirements_ref
    );
}

#[test]
fn module_aliases_sharing_resource_type_route_to_distinct_host_operations() {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_module_operation(
            ["inbox", "work"],
            "Inbox",
            "send",
            "inbox__work__send",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    catalog
        .add_module_operation(
            ["inbox", "personal"],
            "Inbox",
            "send",
            "inbox__personal__send",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");

    assert_eq!(
        catalog
            .resolve_module_operation("Inbox", "inbox.work", "send")
            .map(|binding| binding.host_operation),
        Some("inbox__work__send")
    );
    assert_eq!(
        catalog
            .resolve_module_operation("Inbox", "inbox.personal", "send")
            .map(|binding| binding.host_operation),
        Some("inbox__personal__send")
    );
}

#[test]
fn conflicting_module_operation_binding_returns_a_typed_error() {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_module_operation(
            ["directory"],
            "Directory",
            "lookup",
            "first",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("first binding is valid");

    let error = catalog
        .add_module_operation(
            ["directory"],
            "Directory",
            "lookup",
            "second",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect_err("conflicting dispatch must be rejected");

    assert_eq!(
        error,
        LashlangHostCatalogError::ConflictingModuleOperation {
            module: "directory".to_string(),
            operation: "lookup".to_string(),
            existing: "first".to_string(),
            incoming: "second".to_string(),
        }
    );
}

#[test]
fn identical_module_operation_binding_is_refused_by_name() {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_module_operation(
            ["directory"],
            "Directory",
            "lookup",
            "directory_lookup",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("first operation is valid");
    assert!(matches!(
        catalog.add_module_operation(
            ["directory"],
            "Directory",
            "lookup",
            "directory_lookup",
            TypeExpr::Any,
            TypeExpr::Any,
        ),
        Err(LashlangHostCatalogError::ConflictingModuleOperation { .. })
    ));

    assert_eq!(
        catalog
            .resolve_module_operation("Directory", "directory", "lookup")
            .map(|binding| binding.host_operation),
        Some("directory_lookup")
    );
}

#[test]
fn reusing_module_alias_for_different_resource_type_fails() {
    let mut catalog = LashlangHostCatalog::new();
    catalog
        .add_module_instance(["tools"], "Tools")
        .expect("initial module instance");

    assert!(matches!(
        catalog.add_module_instance(["tools"], "Inbox"),
        Err(LashlangHostCatalogError::ConflictingModuleInstance {
            alias,
            existing,
            incoming,
        }) if alias == "tools" && existing == "Tools" && incoming == "Inbox"
    ));
}
