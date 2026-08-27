#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_union_normalizes_to_null_for_empty_lists() {
        assert_eq!(union_type(Vec::new()), TypeExpr::Null);
    }

    fn resources() -> LashlangHostCatalog {
        let mut catalog = LashlangHostCatalog::new();
        catalog
            .add_module_operation(
                ["tools"],
                "Tools",
                "read_file",
                "read_file",
                TypeExpr::Object(vec![TypeField {
                    name: "path".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                }]),
                TypeExpr::Str,
            )
            .expect("host catalog operation must not conflict");
        catalog
            .add_module_operation(
                ["tools"],
                "Tools",
                "echo",
                "echo",
                TypeExpr::Any,
                TypeExpr::Any,
            )
            .expect("host catalog operation must not conflict");
        for (operation, input_ty) in [
            ("accept_str", TypeExpr::Str),
            ("accept_int", TypeExpr::Int),
            ("accept_float", TypeExpr::Float),
            (
                "accept_mode",
                TypeExpr::Enum(vec!["default".into(), "careful".into()]),
            ),
        ] {
            catalog
                .add_module_operation(
                    ["tools"],
                    "Tools",
                    operation,
                    operation,
                    input_ty,
                    TypeExpr::Null,
                )
                .expect("host catalog operation must not conflict");
        }
        catalog
            .add_module_operation(
                ["tools"],
                "Tools",
                "accept_config",
                "accept_config",
                TypeExpr::Object(vec![TypeField {
                    name: "mode".into(),
                    ty: TypeExpr::Enum(vec!["default".into()]),
                    optional: false,
                }]),
                TypeExpr::Null,
            )
            .expect("host catalog operation must not conflict");
        crate::add_trigger_resource_operations(&mut catalog)
            .expect("trigger resource operations are unique");
        catalog
            .add_trigger_source_constructor(
                ["timer", "Schedule"],
                TypeExpr::Object(vec![
                    TypeField {
                        name: "expr".into(),
                        ty: TypeExpr::Str,
                        optional: false,
                    },
                    TypeField {
                        name: "tz".into(),
                        ty: TypeExpr::Str,
                        optional: true,
                    },
                ]),
                NamedDataType::object(
                    "timer.Tick",
                    vec![TypeField {
                        name: "fired_at".into(),
                        ty: TypeExpr::Str,
                        optional: false,
                    }],
                )
                .expect("valid timer tick type"),
            )
            .expect("valid timer trigger source");
        catalog
    }

    fn full_host_environment() -> LashlangHostEnvironment {
        LashlangHostEnvironment::new(resources(), LashlangAbilities::all())
    }

    fn full_label_environment() -> LashlangHostEnvironment {
        full_host_environment()
            .with_language_features(LashlangLanguageFeatures::default().with_label_annotations())
    }

    #[test]
    fn typescript_lowering_intrinsics_link_through_the_production_registry_path() {
        let builtin = |name: &str, args: Vec<Expr>| Expr::BuiltinCall {
            name: name.into(),
            args,
        };
        let function = || {
            Expr::Function(Box::new(crate::FunctionExpr {
                name: None,
                params: vec!["value".into()],
                captures: Vec::new(),
                body: Box::new(Expr::Return(Box::new(Expr::Variable("value".into())))),
            }))
        };
        let cases = [
            (
                "instanceof",
                Program::block(vec![Expr::Finish(Box::new(builtin(
                    "__typescript_heap_instanceof",
                    vec![
                        builtin(
                            "__typescript_heap_new",
                            vec![Expr::String("TypeError".into())],
                        ),
                        Expr::String("TypeError".into()),
                    ],
                )))]),
            ),
            (
                "global delete",
                Program::block(vec![Expr::Finish(Box::new(builtin(
                    "__typescript_global_delete",
                    vec![Expr::String("state".into())],
                )))]),
            ),
            (
                "global presence",
                Program::block(vec![Expr::Finish(Box::new(builtin(
                    "__typescript_global_has",
                    vec![Expr::String("state".into())],
                )))]),
            ),
            (
                "spread dynamic call",
                Program::block(vec![Expr::Finish(Box::new(builtin(
                    "__typescript_call_dynamic",
                    vec![function(), Expr::List(vec![Expr::Number(1.0)])],
                )))]),
            ),
            (
                "async map",
                Program::block(vec![Expr::Finish(Box::new(builtin(
                    "__typescript_async_map",
                    vec![Expr::List(vec![Expr::Number(1.0)]), function()],
                )))]),
            ),
            (
                "default and rest closure metadata",
                Program::block(vec![Expr::Finish(Box::new(builtin(
                    "__typescript_closure",
                    vec![function(), Expr::Number(1.0), Expr::Bool(false)],
                )))]),
            ),
            (
                "nested global set",
                Program::block(vec![Expr::Finish(Box::new(builtin(
                    "__typescript_global_set",
                    vec![Expr::String("state".into()), Expr::Number(1.0)],
                )))]),
            ),
            (
                "URI codec globals",
                Program::block(vec![Expr::Finish(Box::new(Expr::List(
                    [
                        "__typescript_encode_uri_component",
                        "__typescript_decode_uri_component",
                        "__typescript_encode_uri",
                        "__typescript_decode_uri",
                    ]
                    .into_iter()
                    .map(|name| builtin(name, vec![Expr::String("value".into())]))
                    .collect(),
                )))]),
            ),
        ];
        for (shape, program) in cases {
            LinkedModule::link_with_dialect(
                program,
                full_host_environment(),
                crate::CompilationDialect::Typescript,
            )
            .unwrap_or_else(|error| panic!("{shape} must link: {error}"));
        }
    }

    fn timer_tick_type_with_field(field: &'static str) -> NamedDataType {
        NamedDataType::object(
            "timer.Tick",
            vec![TypeField {
                name: field.into(),
                ty: TypeExpr::Str,
                optional: false,
            }],
        )
        .expect("valid timer tick type")
    }

    fn resources_with_timer_event(event_type: NamedDataType) -> LashlangHostCatalog {
        let mut catalog = LashlangHostCatalog::new();
        crate::add_trigger_resource_operations(&mut catalog)
            .expect("trigger resource operations are unique");
        catalog
            .add_trigger_source_constructor(
                ["timer", "Schedule"],
                TypeExpr::Object(vec![TypeField {
                    name: "expr".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                }]),
                event_type,
            )
            .expect("valid timer trigger source");
        catalog
    }

    #[test]
    fn named_host_data_type_validation_rejects_invalid_shapes() {
        let duplicate_field = NamedDataType::object(
            "timer.Tick",
            vec![
                TypeField {
                    name: "fired_at".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                },
                TypeField {
                    name: "fired_at".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                },
            ],
        )
        .expect_err("duplicate fields should be rejected");
        assert!(matches!(
            duplicate_field,
            NamedDataTypeError::DuplicateField { .. }
        ));

        let nested_ref = NamedDataType::object(
            "timer.Tick",
            vec![TypeField {
                name: "nested".into(),
                ty: TypeExpr::Ref("Other.Type".into()),
                optional: false,
            }],
        )
        .expect_err("nested refs should be rejected");
        assert!(matches!(nested_ref, NamedDataTypeError::NestedRef { .. }));

        let duplicate_enum = NamedDataType::object(
            "timer.Tick",
            vec![TypeField {
                name: "kind".into(),
                ty: TypeExpr::Enum(vec!["Red".into(), "Red".into()]),
                optional: false,
            }],
        )
        .expect_err("duplicate enum values should be rejected");
        assert!(matches!(
            duplicate_enum,
            NamedDataTypeError::DuplicateEnumValue { .. }
        ));

        let simple_name = NamedDataType::object("Tick", vec![])
            .expect_err("host data type names must be qualified");
        assert!(matches!(
            simple_name,
            NamedDataTypeError::InvalidName { .. }
        ));
    }

    #[test]
    fn resource_catalog_rejects_conflicting_named_host_data_type_definitions() {
        let mut catalog = LashlangHostCatalog::new();
        catalog
            .add_named_data_type(timer_tick_type_with_field("fired_at"))
            .expect("first definition");
        let err = catalog
            .add_named_data_type(timer_tick_type_with_field("delivered_at"))
            .expect_err("same host type name with different shape should be rejected");

        assert!(matches!(
            err,
            LashlangHostCatalogError::ConflictingNamedDataType { .. }
        ));
    }

    #[test]
    fn linked_module_accepts_named_processes_resource_params_and_activations() {
        let program = crate::parse(
            r#"
            type ChangeEvent = { path: str }
            process scan(tool: Tools, event: ChangeEvent) {
              text = await tool.read_file({ path: "changed.txt" })?
              finish text
            }
            process watcher(run: any) signals { ready: any } {
              sleep for "0ms"
              signal = wait_signal("ready")
              signal_run(run, "ready", signal)
              finish signal
            }
            process from_tick(tick: timer.Tick) {
              finish tick.fired_at
            }
            source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
            handle = await triggers.register({
              source: source,
              target: from_tick,
              inputs: { tick: trigger.event },
              name: "changed"
            })?
            finish handle
            "#,
        )
        .expect("parse module");

        let linked = LinkedModule::link(program, full_host_environment()).expect("link module");

        assert!(
            linked
                .module_ref
                .as_str()
                .starts_with("lashlang:v2:blake3:")
        );
    }

    #[test]
    fn trigger_mutation_rejects_non_literal_subscription_key() {
        let program = crate::parse(
            r#"
            key = "daily-digest"
            await triggers.disable({ subscription_key: key, expected_revision: 1 })?
            finish true
            "#,
        )
        .expect("parse module");
        let error = LinkedModule::link(program, full_host_environment())
            .expect_err("trigger keys must be literals");
        assert!(matches!(
            error,
            LinkError::InvalidTriggerSubscriptionKey { .. }
        ));
    }

    #[test]
    fn trigger_mutation_rejects_reserved_subscription_key_prefix() {
        let program = crate::parse(
            r#"
            await triggers.delete({
              subscription_key: "lash.internal/manager",
              expected_revision: 1
            })?
            finish true
            "#,
        )
        .expect("parse module");
        let error = LinkedModule::link(program, full_host_environment())
            .expect_err("internal trigger prefix is reserved");
        assert!(matches!(
            error,
            LinkError::InvalidTriggerSubscriptionKey { .. }
        ));
    }

    #[test]
    fn linked_module_accepts_restate_board_process_with_imported_schemas() {
        let mut catalog = LashlangHostCatalog::new();
        let read_input = crate::json_schema_to_type_expr(&serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }));
        let read_output = crate::json_schema_to_type_expr(&serde_json::json!({ "type": "object" }));
        let play_input = crate::json_schema_to_type_expr(&serde_json::json!({
            "type": "object",
            "properties": {
                "cell": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 8
                }
            },
            "required": ["cell"],
            "additionalProperties": false
        }));
        let play_output = crate::json_schema_to_type_expr(&serde_json::json!({ "type": "object" }));

        assert_eq!(read_output, TypeExpr::Dict);
        assert_eq!(
            play_input,
            TypeExpr::Object(vec![TypeField {
                name: "cell".into(),
                ty: TypeExpr::Int,
                optional: false,
            }])
        );
        catalog
            .add_module_operation(["board"], "Board", "read", "read", read_input, read_output)
            .expect("host catalog operation must not conflict");
        catalog
            .add_module_operation(["board"], "Board", "play", "play", play_input, play_output)
            .expect("host catalog operation must not conflict");
        let environment = LashlangHostEnvironment::new(catalog, LashlangAbilities::all());
        let program = crate::parse(
            r#"
            process play_center_once(board_tool: Board) {
              state = await board_tool.read({})?
              if state.turn == "O" and contains(state.legal_moves, 4) {
                move = await board_tool.play({ cell: 4 })?
                finish { before: state, move: move, played: true }
              } else { finish { before: state, played: false } }
            }
            handle = start play_center_once(board_tool: board)
            result = (await handle)?
            finish "done via Restate E2E"
            "#,
        )
        .expect("parse Restate board process");

        LinkedModule::link(program, environment.clone())
            .expect("link Restate board process with imported schemas");

        let fractional =
            crate::parse("await board.play({ cell: 4.5 })?").expect("parse fractional board call");
        assert!(matches!(
            LinkedModule::link(fractional, environment),
            Err(LinkError::IncompatibleOperationInput { expected, actual, .. })
                if expected == "{ cell: int }" && actual == "{ cell: float }"
        ));
    }

    #[test]
    fn linked_module_allows_trigger_registration_name_to_match_target_process() {
        let program = crate::parse(
            r#"
            process changed(tick: timer.Tick) {
              finish true
            }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: changed,
              inputs: { tick: trigger.event },
              name: "changed"
            })?
            "#,
        )
        .expect("parse module");

        LinkedModule::link(program, full_host_environment())
            .expect("trigger registration names and process names occupy different namespaces");
    }

    #[test]
    fn linked_module_resolves_host_named_data_refs_for_fields_and_structural_assignability() {
        let direct_ref = crate::parse(
            r#"
            process from_tick(tick: timer.Tick) {
              finish tick.fired_at
            }
            finish true
            "#,
        )
        .expect("parse direct host data ref");
        LinkedModule::link(direct_ref, full_host_environment())
            .expect("host data ref fields should link");

        let structural_input = crate::parse(
            r#"
            process from_tick(tick: { fired_at: str }) {
              finish tick.fired_at
            }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: from_tick,
              inputs: { tick: trigger.event }
            })?
            "#,
        )
        .expect("parse structural target input");
        LinkedModule::link(structural_input, full_host_environment())
            .expect("host data shape should be structurally assignable");
    }

    #[test]
    fn linked_module_rejects_unknown_host_data_refs_and_opaque_source_field_access() {
        let unknown = crate::parse(
            r#"
            process from_tick(tick: foo.Tick) {
              finish true
            }
            finish true
            "#,
        )
        .expect("parse unknown host type");
        assert!(matches!(
            LinkedModule::link(unknown, full_host_environment()),
            Err(LinkError::UnknownType { name, .. }) if name == "foo.Tick"
        ));

        let opaque = crate::parse(
            r#"
            source = timer.Schedule({ expr: "0 8 * * *" })
            finish source.expr
            "#,
        )
        .expect("parse opaque source access");
        assert!(matches!(
            LinkedModule::link(opaque, full_host_environment()),
            Err(LinkError::OpaqueHostDescriptorAccess { type_name, .. }) if type_name == "timer.Schedule"
        ));
    }

    #[test]
    fn host_requirements_ref_tracks_host_named_data_type_shape_changes() {
        let program = crate::parse(
            r#"
            process from_tick(tick: any) {
              finish true
            }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: from_tick,
              inputs: { tick: trigger.event }
            })?
            "#,
        )
        .expect("parse trigger registration");
        let first = LinkedModule::link(
            program.clone(),
            LashlangHostEnvironment::new(
                resources_with_timer_event(timer_tick_type_with_field("fired_at")),
                LashlangAbilities::all(),
            ),
        )
        .expect("link first trigger occurrence shape");
        let second = LinkedModule::link(
            program,
            LashlangHostEnvironment::new(
                resources_with_timer_event(timer_tick_type_with_field("delivered_at")),
                LashlangAbilities::all(),
            ),
        )
        .expect("link changed trigger occurrence shape");

        assert_ne!(first.host_requirements_ref, second.host_requirements_ref);
    }

    #[test]
    fn linked_module_accepts_top_level_sleep() {
        let program = crate::parse("sleep for 1").expect("parse sleep");

        LinkedModule::link(program, full_host_environment()).expect("top-level sleep should link");
    }

    #[test]
    fn linked_module_rejects_process_lifecycle_outside_process_body() {
        let program = crate::parse("payload = wait_signal(\"ready\")").expect("parse wait_signal");

        let err = LinkedModule::link(program, full_host_environment())
            .expect_err("top-level process lifecycle should be rejected");

        assert!(
            matches!(
                err,
                LinkError::ProcessLifecycleOutsideProcess {
                    keyword: "wait_signal",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn linked_module_accepts_top_level_signal_run() {
        // `signal_run` (sending) mirrors `await` / `cancel`: legal from the
        // foreground turn, unlike the process-only `wait_signal`.
        let program =
            crate::parse("signal_run(\"handle\", \"ready\", \"ping\")").expect("parse signal_run");

        LinkedModule::link(program, full_host_environment())
            .expect("top-level signal_run should link");
    }

    #[test]
    fn linked_module_rejects_bad_process_args_and_unresolved_operations() {
        let missing_arg = crate::parse(
            r#"
            process scan(tool: Tools, path: str) { finish path }
            start scan(tool: tools)
            "#,
        )
        .expect("parse missing arg");
        assert!(matches!(
            LinkedModule::link(missing_arg, full_host_environment()),
            Err(LinkError::MissingProcessArgument { arg, .. }) if arg == "path"
        ));

        let bad_operation = crate::parse(
            r#"
            process scan(tool: Tools) {
              finish await tool.missing({})?
            }
            "#,
        )
        .expect("parse bad operation");
        assert!(matches!(
            LinkedModule::link(bad_operation, full_host_environment()),
            Err(LinkError::UnknownResourceOperation { operation, .. }) if operation == "missing"
        ));
    }

    #[test]
    fn linked_module_rejects_disabled_abilities() {
        let process =
            crate::parse("process worker() { finish null }").expect("parse disabled process");
        assert!(matches!(
            LinkedModule::link(
                process,
                LashlangHostEnvironment::new(resources(), LashlangAbilities::default())
            ),
            Err(LinkError::FeatureDisabled {
                feature: "processes",
                ..
            })
        ));

        let start = crate::parse("start worker()").expect("parse disabled start");
        assert!(matches!(
            LinkedModule::link(
                start,
                LashlangHostEnvironment::new(resources(), LashlangAbilities::default())
            ),
            Err(LinkError::FeatureDisabled {
                feature: "processes",
                ..
            })
        ));

        let sleep = crate::parse("sleep for \"1s\"").expect("parse disabled sleep");
        assert!(matches!(
            LinkedModule::link(
                sleep,
                LashlangHostEnvironment::new(resources(), LashlangAbilities::default())
            ),
            Err(LinkError::FeatureDisabled {
                feature: "sleep",
                ..
            })
        ));

        let signal = crate::parse(
            "process worker() signals { ready: any } { payload = wait_signal(\"ready\") }",
        )
        .expect("parse disabled process signal");
        assert!(matches!(
            LinkedModule::link(
                signal,
                LashlangHostEnvironment::new(
                    resources(),
                    LashlangAbilities::default().with_processes()
                )
            ),
            Err(LinkError::FeatureDisabled {
                feature: "process signals",
                ..
            })
        ));

        let trigger = crate::parse(
            r#"
            process worker(tick: timer.Tick) { finish true }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: worker,
              inputs: { tick: trigger.event }
            })?
            "#,
        )
        .expect("parse disabled trigger");
        assert!(matches!(
            LinkedModule::link(
                trigger,
                LashlangHostEnvironment::new(
                    resources(),
                    LashlangAbilities::default().with_processes()
                )
            ),
            Err(LinkError::FeatureDisabled {
                feature: "triggers",
                ..
            })
        ));
    }

    #[test]
    fn linked_module_validates_value_constructors_and_trigger_registry_ops() {
        let program = crate::parse(
            r#"
            process scan(tick: timer.Tick) -> bool {
              finish true
            }
            source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
            handle = await triggers.register({
              source: source,
              target: scan,
              inputs: { tick: trigger.event },
              name: "scan",
              subscription_key: "scan"
            })?
            registrations = await triggers.list({ target: scan })?
            disabled = await triggers.disable({
              subscription_key: "scan",
              expected_revision: registrations[0].revision
            })?
            finish { handle: handle, registrations: registrations, disabled: disabled }
            "#,
        )
        .expect("parse trigger registry program");
        assert!(LinkedModule::link(program, full_host_environment()).is_ok());
    }

    #[test]
    fn linked_module_accepts_explicit_trigger_input_mappings() {
        let repeated_event = crate::parse(
            r#"
            process scan(a: timer.Tick, b: { fired_at: str }) {
              finish { a: a.fired_at, b: b.fired_at }
            }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: scan,
              inputs: { a: trigger.event, b: trigger.event }
            })?
            "#,
        )
        .expect("parse repeated event mapping");
        LinkedModule::link(repeated_event, full_host_environment())
            .expect("event payload should map to multiple assignable params");

        let fixed_authority = crate::parse(
            r#"
            process scan(tick: timer.Tick, tool: Tools) {
              text = await tool.read_file({ path: tick.fired_at })?
              finish text
            }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: scan,
              inputs: { tick: trigger.event, tool: tools }
            })?
            "#,
        )
        .expect("parse fixed authority mapping");
        LinkedModule::link(fixed_authority, full_host_environment())
            .expect("fixed resource inputs should satisfy process authority params");
    }

    #[test]
    fn linked_module_rejects_colliding_default_trigger_keys() {
        let program = crate::parse(
            r#"
            process scan(tick: timer.Tick) {
              finish tick.fired_at
            }
            first = timer.Schedule({ expr: "0 8 * * *" })
            second = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: first,
              target: scan,
              inputs: { tick: trigger.event }
            })?
            await triggers.register({
              source: second,
              target: scan,
              inputs: { tick: trigger.event }
            })?
            "#,
        )
        .expect("parse duplicate default registrations");

        let error = LinkedModule::link(program, full_host_environment())
            .expect_err("duplicate derived keys must fail linking");
        assert!(matches!(
            &error,
            LinkError::DuplicateDerivedTriggerSubscriptionKey {
                process,
                source_type,
                ..
            } if process == "scan" && source_type == "timer.Schedule"
        ));
        assert!(
            error
                .to_string()
                .contains("explicit literal subscription_key")
        );
    }

    #[test]
    fn linked_module_allows_explicit_keys_for_default_key_collision_shape() {
        let program = crate::parse(
            r#"
            process scan(tick: timer.Tick) {
              finish tick.fired_at
            }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: scan,
              inputs: { tick: trigger.event },
              subscription_key: "morning-scan-primary"
            })?
            await triggers.register({
              source: source,
              target: scan,
              inputs: { tick: trigger.event },
              subscription_key: "morning-scan-secondary"
            })?
            "#,
        )
        .expect("parse explicit duplicate-shape registrations");

        LinkedModule::link(program, full_host_environment())
            .expect("explicit literal keys disambiguate registration sites");
    }

    #[test]
    fn linked_artifact_manifest_contains_explicit_and_materialized_keys() {
        let source = serde_json::json!({ "expr": "0 8 * * *" });
        let source_key = semantic_trigger_source_key("timer.Schedule", &source);
        let derived_key = semantic_trigger_subscription_key("scan", "timer.Schedule", &source_key);
        let program = crate::parse(
            r#"
            process scan(tick: timer.Tick) {
              finish tick.fired_at
            }
            morning = timer.Schedule({ expr: "0 8 * * *" })
            evening = timer.Schedule({ expr: "0 18 * * *" })
            await triggers.register({
              source: morning,
              target: scan,
              inputs: { tick: trigger.event }
            })?
            await triggers.register({
              source: evening,
              target: scan,
              inputs: { tick: trigger.event },
              subscription_key: "evening-scan"
            })?
            "#,
        )
        .expect("parse trigger manifest module");
        let linked =
            LinkedModule::link(program, full_host_environment()).expect("link manifest module");

        assert_eq!(
            linked.artifact.trigger_key_manifest.subscription_keys,
            BTreeSet::from([derived_key.clone(), "evening-scan".to_string()])
        );
        let canonical = linked
            .artifact
            .canonical_source()
            .expect("canonical linked source");
        assert!(
            canonical.contains(&format!("subscription_key: \"{derived_key}\"")),
            "{canonical}"
        );
    }

    #[test]
    fn linked_module_captures_concrete_process_body_resources_statically() {
        let program = crate::parse(
            r#"
            process scan(tick: timer.Tick) {
              text = await tools.read_file({ path: tick.fired_at })?
              finish text
            }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: scan,
              inputs: { tick: trigger.event }
            })?
            "#,
        )
        .expect("parse captured authority process");
        let linked = LinkedModule::link(program, full_host_environment())
            .expect("process body should capture concrete host resources");
        let process = linked
            .artifact
            .canonical_ir
            .process("scan")
            .expect("scan process");
        fn contains_resource_ref(expr: &Expr, path: &str) -> bool {
            matches!(expr, Expr::ResourceRef(resource) if resource.path_string() == path)
                || expr
                    .children()
                    .any(|child| contains_resource_ref(child, path))
        }
        assert!(
            contains_resource_ref(&process.body, "tools"),
            "linked process body should contain a persisted tools resource ref"
        );

        let shadowed = crate::parse(
            r#"
            tool = tools
            process scan(tick: timer.Tick) {
              text = await tool.read_file({ path: tick.fired_at })?
              finish text
            }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: scan,
              inputs: { tick: trigger.event }
            })?
            "#,
        )
        .expect("parse foreground variable capture");
        assert!(matches!(
            LinkedModule::link(shadowed, full_host_environment()),
            Err(LinkError::UnknownName { name, .. }) if name == "tool"
        ));
    }

    #[test]
    fn linked_module_accepts_button_trigger_source_constructor() {
        let mut resources = resources();
        resources
            .add_trigger_source_constructor(
                ["ui", "button", "pressed"],
                TypeExpr::Object(vec![]),
                NamedDataType::object(
                    "ui.button.Pressed",
                    vec![
                        TypeField {
                            name: "button".into(),
                            ty: TypeExpr::Union(vec![
                                TypeExpr::Enum(vec!["Red".into()]),
                                TypeExpr::Enum(vec!["Blue".into()]),
                            ]),
                            optional: false,
                        },
                        TypeField {
                            name: "message".into(),
                            ty: TypeExpr::Str,
                            optional: false,
                        },
                        TypeField {
                            name: "pressed_at".into(),
                            ty: TypeExpr::Str,
                            optional: false,
                        },
                    ],
                )
                .expect("valid button event type"),
            )
            .expect("valid button trigger source");
        let program = crate::parse(
            r#"
            process on_button(event: ui.button.Pressed) {
              wake { kind: "button_pressed", button: event.button, message: event.message }
              finish true
            }

            handle = await triggers.register({
              source: ui.button.pressed({}),
              target: on_button,
              inputs: { event: trigger.event },
              name: "button watcher"
            })?
            finish handle
            "#,
        )
        .expect("parse button trigger source");

        LinkedModule::link(
            program,
            LashlangHostEnvironment::new(resources, LashlangAbilities::all()),
        )
        .expect("button trigger source should link");
    }

    #[test]
    fn linked_module_rejects_bad_trigger_registry_bindings() {
        let missing = crate::parse(
            r#"
            process scan(tick: timer.Tick) { finish true }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({ target: scan })?
            "#,
        )
        .expect("parse missing source");
        assert!(matches!(
            LinkedModule::link(missing, full_host_environment()),
            Err(LinkError::InvalidTriggerRegistration { .. })
        ));

        let missing_inputs = crate::parse(
            r#"
            process scan(tick: timer.Tick) { finish true }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({ source: source, target: scan })?
            "#,
        )
        .expect("parse missing inputs");
        assert!(matches!(
            LinkedModule::link(missing_inputs, full_host_environment()),
            Err(LinkError::InvalidTriggerRegistration { .. })
        ));

        let wrong_source = crate::parse(
            r#"
            process scan(tick: timer.Tick) { finish true }
            await triggers.register({
              source: { expr: "0 8 * * *" },
              target: scan,
              inputs: { tick: trigger.event }
            })?
            "#,
        )
        .expect("parse wrong source");
        assert!(matches!(
            LinkedModule::link(wrong_source, full_host_environment()),
            Err(LinkError::UnknownTriggerEventType { .. })
        ));

        let payload_mismatch = crate::parse(
            r#"
            process scan(tick: str) { finish tick }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: scan,
              inputs: { tick: trigger.event }
            })?
            "#,
        )
        .expect("parse payload mismatch");
        assert!(matches!(
            LinkedModule::link(payload_mismatch, full_host_environment()),
            Err(LinkError::TriggerEventMismatch { .. })
        ));

        let unknown_input = crate::parse(
            r#"
            process scan(tick: timer.Tick) { finish true }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: scan,
              inputs: { tick: trigger.event, extra: "nope" }
            })?
            "#,
        )
        .expect("parse unknown input");
        assert!(matches!(
            LinkedModule::link(unknown_input, full_host_environment()),
            Err(LinkError::UnknownTriggerInput { input, .. }) if input == "extra"
        ));

        let duplicate_input = crate::parse(
            r#"
            process scan(tick: timer.Tick) { finish true }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: scan,
              inputs: { tick: trigger.event, tick: trigger.event }
            })?
            "#,
        )
        .expect("parse duplicate input");
        assert!(matches!(
            LinkedModule::link(duplicate_input, full_host_environment()),
            Err(LinkError::DuplicateTriggerInput { input, .. }) if input == "tick"
        ));

        let no_event_input = crate::parse(
            r#"
            process scan(tick: timer.Tick, label: str) { finish label }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: scan,
              inputs: { tick: { fired_at: "static" }, label: "static" }
            })?
            "#,
        )
        .expect("parse no event input");
        assert!(matches!(
            LinkedModule::link(no_event_input, full_host_environment()),
            Err(LinkError::MissingTriggerEventInput { .. })
        ));

        let event_projection = crate::parse(
            r#"
            process scan(fired_at: str) { finish fired_at }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: scan,
              inputs: { fired_at: trigger.event.fired_at }
            })?
            "#,
        )
        .expect("parse event projection");
        assert!(matches!(
            LinkedModule::link(event_projection, full_host_environment()),
            Err(LinkError::TriggerEventProjection { .. })
        ));

        let event_outside_inputs = crate::parse(
            r#"
            process scan(tick: timer.Tick) { finish true }
            finish trigger.event
            "#,
        )
        .expect("parse event outside inputs");
        assert!(matches!(
            LinkedModule::link(event_outside_inputs, full_host_environment()),
            Err(LinkError::TriggerEventOutsideInputs { .. })
        ));

        let multi_input = crate::parse(
            r#"
            process scan(tick: timer.Tick, extra: str) { finish extra }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: scan,
              inputs: { tick: trigger.event }
            })?
            "#,
        )
        .expect("parse multi-input target");
        assert!(matches!(
            LinkedModule::link(multi_input, full_host_environment()),
            Err(LinkError::MissingTriggerInput { input, .. }) if input == "extra"
        ));

        let target_is_not_process = crate::parse(
            r#"
            process scan(tick: timer.Tick) { finish true }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: source,
              inputs: { tick: trigger.event }
            })?
            "#,
        )
        .expect("parse non-process target");
        assert!(matches!(
            LinkedModule::link(target_is_not_process, full_host_environment()),
            Err(LinkError::InvalidTriggerTarget { .. })
        ));

        let list_without_filters = crate::parse(
            r#"
            process scan(tick: timer.Tick) { finish true }
            await triggers.list({})?
            "#,
        )
        .expect("parse trigger list without filters");
        assert!(LinkedModule::link(list_without_filters, full_host_environment()).is_ok());

        let list_with_filters = crate::parse(
            r#"
            process scan(tick: timer.Tick) { finish true }
            await triggers.list({
              target: scan,
              name: "daily",
              source_type: "timer.Schedule",
              enabled: true
            })?
            "#,
        )
        .expect("parse trigger list filters");
        assert!(LinkedModule::link(list_with_filters, full_host_environment()).is_ok());

        let list_target_is_not_process = crate::parse(
            r#"
            process scan(tick: timer.Tick) { finish true }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.list({ target: source })?
            "#,
        )
        .expect("parse trigger list non-process target");
        assert!(matches!(
            LinkedModule::link(list_target_is_not_process, full_host_environment()),
            Err(LinkError::InvalidTriggerTarget { .. })
                | Err(LinkError::IncompatibleOperationInput { .. })
        ));

        let constructor_mismatch = crate::parse(
            r#"
            source = timer.Schedule({ expr: 1 })
            finish source
            "#,
        )
        .expect("parse constructor mismatch");
        assert!(matches!(
            LinkedModule::link(constructor_mismatch, full_host_environment()),
            Err(LinkError::IncompatibleConstructorInput { .. })
        ));

        let operation_mismatch = crate::parse(
            r#"
            await tools.read_file({ path: 1 })?
            "#,
        )
        .expect("parse operation mismatch");
        assert!(matches!(
            LinkedModule::link(operation_mismatch, full_host_environment()),
            Err(LinkError::IncompatibleOperationInput { .. })
        ));
    }

    #[test]
    fn linked_module_infers_process_output_and_validates_return_annotations() {
        let inferred = crate::parse(
            r#"
            process done(tick: timer.Tick) -> bool {
              finish true
            }
            source = timer.Schedule({ expr: "0 8 * * *" })
            await triggers.register({
              source: source,
              target: done,
              inputs: { tick: trigger.event }
            })?
            "#,
        )
        .expect("parse inferred output");
        assert!(LinkedModule::link(inferred, full_host_environment()).is_ok());

        let union_mismatch = crate::parse(
            r#"
            process done(tick: timer.Tick) -> bool {
              if true {
                finish true
              }
              finish "done"
            }
            "#,
        )
        .expect("parse union mismatch");
        assert!(matches!(
            LinkedModule::link(union_mismatch, full_host_environment()),
            Err(LinkError::IncompatibleProcessReturn { .. })
        ));
    }

    #[test]
    fn linked_module_hash_ignores_unused_host_abilities() {
        let program = crate::parse("finish 1").expect("parse");
        let minimal = LinkedModule::link(
            program.clone(),
            LashlangHostEnvironment::new(resources(), LashlangAbilities::default()),
        )
        .expect("link minimal");
        let processes = LinkedModule::link(
            program,
            LashlangHostEnvironment::new(
                resources(),
                LashlangAbilities::default().with_processes(),
            ),
        )
        .expect("link process ability");

        assert_eq!(minimal.module_ref, processes.module_ref);
        assert_eq!(
            minimal.host_requirements_ref,
            processes.host_requirements_ref
        );
    }

    #[test]
    fn label_annotations_require_enabled_language_feature() {
        let program = crate::parse(
            r#"
            @label(title: "Scan files")
            process scan(tool: Tools) {
              @label(title: "Read file")
              text = await tool.read_file({ path: "." })?
              finish text
            }
            "#,
        )
        .expect("parse annotated process");

        let err = LinkedModule::link(program.clone(), full_host_environment())
            .expect_err("default surface should reject label annotations");
        assert!(matches!(
            err,
            LinkError::FeatureDisabled {
                feature: "label annotations",
                ..
            }
        ));

        let linked = LinkedModule::link(program, full_label_environment())
            .expect("enabled surface should link");
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
        let source = "count = 1\n@label(title: \"Finish up\") finish count\n";
        let err = LinkedModule::link(
            crate::parse(source).expect("parse annotated main"),
            full_host_environment(),
        )
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
    fn label_annotation_text_inside_strings_does_not_require_feature() {
        let linked = LinkedModule::link(
            crate::parse(r####"finish r"""@label(title: "Plain text")""""####)
                .expect("parse string"),
            full_host_environment(),
        )
        .expect("disabled label annotations should not reject string text");

        assert!(
            !linked
                .artifact
                .host_requirements
                .language_features
                .label_annotations
        );
    }

    #[test]
    fn label_metadata_round_trips_and_changes_artifact_identity() {
        let first = LinkedModule::link(
            crate::parse(
                r#"
                @label(title: "Scan files")
                process scan(tool: Tools) {
                  @label(title: "Read file", description: "Load source text")
                  text = await tool.read_file({ path: "." })?
                  @label(title: "Finish")
                  finish text
                }
                "#,
            )
            .expect("parse first"),
            full_label_environment(),
        )
        .expect("link first");
        let changed = LinkedModule::link(
            crate::parse(
                r#"
                @label(title: "Scan files")
                process scan(tool: Tools) {
                  @label(title: "Read source", description: "Load source text")
                  text = await tool.read_file({ path: "." })?
                  @label(title: "Finish")
                  finish text
                }
                "#,
            )
            .expect("parse changed"),
            full_label_environment(),
        )
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
        let compact = LinkedModule::link(
            crate::parse("process scan(root: str) { finish root }").expect("parse compact"),
            full_host_environment(),
        )
        .expect("link compact");
        let formatted = LinkedModule::link(
            crate::parse(
                r#"
                process scan(root: str) {
                    finish root
                }
                "#,
            )
            .expect("parse formatted"),
            full_host_environment(),
        )
        .expect("link formatted");

        assert_eq!(compact.module_ref, formatted.module_ref);
    }

    #[test]
    fn process_ref_tracks_abi_and_body_but_not_local_binder_names() {
        let original = LinkedModule::link(
            crate::parse("process scan(root: str) { value = root\nfinish value }")
                .expect("parse original"),
            full_host_environment(),
        )
        .expect("link original");
        let renamed_local = LinkedModule::link(
            crate::parse("process scan(root: str) { renamed = root\nfinish renamed }")
                .expect("parse renamed local"),
            full_host_environment(),
        )
        .expect("link renamed local");
        let renamed_param = LinkedModule::link(
            crate::parse("process scan(path: str) { value = path\nfinish value }")
                .expect("parse renamed param"),
            full_host_environment(),
        )
        .expect("link renamed param");
        let changed_body = LinkedModule::link(
            crate::parse("process scan(root: str) { value = root\nfinish { value: value } }")
                .expect("parse changed body"),
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
        let program = crate::parse(
            "process scan(tool: Tools) { finish (await tool.read_file({ path: \".\" }))? }",
        )
        .expect("parse process");

        let base = LinkedModule::link(program.clone(), full_host_environment()).expect("link base");
        let extra = LinkedModule::link(
            program.clone(),
            LashlangHostEnvironment::new(with_extra, LashlangAbilities::all()),
        )
        .expect("link extra");
        let changed_requirement = LinkedModule::link(
            crate::parse(
                "process scan(tool: Tools) { finish (await tool.echo({ value: \".\" }))? }",
            )
            .expect("parse changed resource"),
            full_host_environment(),
        )
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

    include!("catalog_tests.rs");

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

    // --- behaviour-pinning tests for the single linking walk -------------
    //
    // These lock in the error *set*, *ordering*, and *spans* the linker
    // produced when validation and lowering were two separate passes, so the
    // fold into one walk stays behaviour-preserving.

    #[test]
    fn declaration_errors_report_before_main_errors() {
        // The process body references an unknown name AND the main block
        // references a different unknown name. The declaration error must win.
        let program = crate::parse(
            r#"
            process scan() { finish missing_in_body }
            finish missing_in_main
            "#,
        )
        .expect("parse");
        let err = LinkedModule::link(program, full_host_environment())
            .expect_err("both bodies reference unknowns");
        assert!(
            matches!(&err, LinkError::UnknownName { name, .. } if name == "missing_in_body"),
            "{err:?}"
        );
    }

    #[test]
    fn unknown_name_in_process_body_carries_declaration_span() {
        let program = crate::parse("process scan() { finish missing }").expect("parse");
        let err = LinkedModule::link(program, full_host_environment()).expect_err("unknown name");
        let LinkError::UnknownName { name, span } = &err else {
            panic!("expected UnknownName, got {err:?}");
        };
        assert_eq!(name, "missing");
        assert!(span.is_some(), "declaration-body error should carry a span");
    }

    #[test]
    fn unknown_top_level_name_on_line_40_fails_at_link() {
        let mut lines = (1..40)
            .map(|index| format!("value_{index} = {index}"))
            .collect::<Vec<_>>();
        lines.push("finish value_39_typo".to_string());
        let source = lines.join("\n");
        let program = crate::parse(&source).expect("parse");

        let err = LinkedModule::link(program, full_host_environment())
            .expect_err("top-level typo must fail before execution");
        let LinkError::UnknownName { name, span } = err else {
            panic!("expected UnknownName");
        };
        assert_eq!(name, "value_39_typo");
        let diagnostic =
            crate::format_link_diagnostic(&source, &LinkError::UnknownName { name, span });
        assert!(diagnostic.contains("--> line 40, column 8"), "{diagnostic}");
    }

    #[test]
    fn live_host_globals_are_known_at_top_level() {
        let program =
            crate::parse("finish { saved: persisted, payload: projected }").expect("parse");
        let environment = full_host_environment().with_globals(["persisted", "projected"]);
        LinkedModule::link(program, environment).expect("live host globals should link");
    }

    #[test]
    fn module_operation_calls_win_over_colliding_live_globals() {
        let program = crate::parse("finish await tools.echo({ value: tools })?").expect("parse");
        let environment = full_host_environment().with_globals(["tools"]);
        LinkedModule::link(program, environment)
            .expect("the exact module operation path should remain callable");
    }

    #[test]
    fn linker_reproduces_full_error_set() {
        // One representative source per error variant that the expression walk
        // is responsible for raising.
        // Unknown names are rejected in both declarations and the main block.
        type ErrorCase = (&'static str, fn(&LinkError) -> bool);
        let cases: &[ErrorCase] = &[
            (
                "process scan() { finish missing }",
                |err| matches!(err, LinkError::UnknownName { name, .. } if name == "missing"),
            ),
            (
                "process scan() { missing[0] = 1 }",
                |err| matches!(err, LinkError::UnknownName { name, .. } if name == "missing"),
            ),
            (
                "finish not_a_builtin(1)",
                |err| matches!(err, LinkError::UnknownBuiltin { name, .. } if name == "not_a_builtin"),
            ),
            (
                "x = 1\nfinish x.read_file({})",
                |err| matches!(err, LinkError::UnresolvedReceiver { operation, .. } if operation == "read_file"),
            ),
            (
                "process scan() { finish 1 }\nstart scan(extra: 1)",
                |err| matches!(err, LinkError::UnexpectedProcessArgument { arg, .. } if arg == "extra"),
            ),
            (
                "process scan(needed: str) { finish needed }\nstart scan()",
                |err| matches!(err, LinkError::MissingProcessArgument { arg, .. } if arg == "needed"),
            ),
            (
                "start ghost()",
                |err| matches!(err, LinkError::UnknownProcess { name, .. } if name == "ghost"),
            ),
        ];

        for (source, predicate) in cases {
            let program =
                crate::parse(source).unwrap_or_else(|err| panic!("parse {source:?}: {err}"));
            let err = LinkedModule::link(program, full_host_environment())
                .err()
                .unwrap_or_else(|| panic!("{source:?} should fail to link"));
            assert!(predicate(&err), "unexpected error for {source:?}: {err:?}");
        }
    }

    #[test]
    fn unknown_resource_operation_still_rejected_after_receiver_resolves() {
        let program = crate::parse(
            r#"
            process scan(tool: Tools) { finish await tool.does_not_exist({})? }
            "#,
        )
        .expect("parse");
        let err =
            LinkedModule::link(program, full_host_environment()).expect_err("operation missing");
        assert!(
            matches!(&err, LinkError::UnknownResourceOperation { operation, .. } if operation == "does_not_exist"),
            "{err:?}"
        );
    }

    #[test]
    fn link_diagnostics_render_deduplicated_operation_hints() {
        let unknown_source = "finish await tools.does_not_exist({})?";
        let unknown = LinkedModule::link(
            crate::parse(unknown_source).expect("parse unknown operation"),
            full_host_environment(),
        )
        .expect_err("operation missing");
        let LinkError::UnknownResourceOperation { suggestions, .. } = &unknown else {
            panic!("expected UnknownResourceOperation, got {unknown:?}");
        };
        assert!(
            suggestions.contains(&"tools.echo".to_string()),
            "{suggestions:?}"
        );
        assert!(
            suggestions.contains(&"tools.read_file".to_string()),
            "{suggestions:?}"
        );
        let diagnostic = crate::format_link_diagnostic(unknown_source, &unknown);
        assert!(
            diagnostic.contains("hint: available operations:")
                && diagnostic.contains("`tools.echo`")
                && diagnostic.contains("`tools.read_file`"),
            "{diagnostic}"
        );

        let receiver_source = "value = 1\nfinish await value.echo({})?";
        let receiver = LinkedModule::link(
            crate::parse(receiver_source).expect("parse unresolved receiver"),
            full_host_environment(),
        )
        .expect_err("receiver is not an authority");
        let LinkError::UnresolvedReceiver { suggestions, .. } = &receiver else {
            panic!("expected UnresolvedReceiver, got {receiver:?}");
        };
        assert_eq!(suggestions, &["tools.echo"]);
        let diagnostic = crate::format_link_diagnostic(receiver_source, &receiver);
        assert!(
            diagnostic.contains("hint: use a module authority, e.g. `tools.echo`"),
            "{diagnostic}"
        );
        assert_eq!(diagnostic.matches("tools.echo").count(), 1, "{diagnostic}");
    }

    #[test]
    fn expected_enum_slots_reject_wrong_literals_but_admit_members_and_broad_strings() {
        let wrong =
            crate::parse(r#"await tools.accept_mode("nope")?"#).expect("parse wrong enum literal");
        assert!(matches!(
            LinkedModule::link(wrong, full_host_environment()),
            Err(LinkError::IncompatibleExpectedLiteral { .. })
        ));

        let member =
            crate::parse(r#"await tools.accept_mode("default")?"#).expect("parse enum member");
        LinkedModule::link(member, full_host_environment()).expect("enum member should link");

        let broad = crate::parse(
            r#"
            process forward(mode: str) {
              await tools.accept_mode(mode)?
            }
            "#,
        )
        .expect("parse broad string input");
        LinkedModule::link(broad, full_host_environment())
            .expect("broad strings remain gradually consistent with enums");

        let nested = crate::parse(r#"await tools.accept_config({ mode: "nope" })?"#)
            .expect("parse nested wrong enum literal");
        assert!(matches!(
            LinkedModule::link(nested, full_host_environment()),
            Err(LinkError::IncompatibleExpectedLiteral { .. })
        ));

        let container = crate::parse(
            r#"
            process mutate(state: { mode: enum["default"] }) {
              state.mode = "nope"
            }
            "#,
        )
        .expect("parse container expected type");
        assert!(matches!(
            LinkedModule::link(container, full_host_environment()),
            Err(LinkError::IncompatibleExpectedLiteral { .. })
        ));

        let declared_return =
            crate::parse(r#"process choose() -> enum["default"] { finish "nope" }"#)
                .expect("parse declared enum return");
        assert!(matches!(
            LinkedModule::link(declared_return, full_host_environment()),
            Err(LinkError::IncompatibleExpectedLiteral { .. })
        ));

        let declared_argument = crate::parse(
            r#"
            process select(mode: enum["default"]) { finish mode }
            start select(mode: "nope")
            "#,
        )
        .expect("parse declared enum argument");
        assert!(matches!(
            LinkedModule::link(declared_argument, full_host_environment()),
            Err(LinkError::IncompatibleExpectedLiteral { .. })
        ));
    }

    #[test]
    fn union_expected_types_do_not_treat_dict_as_accepting_string_literals() {
        let program = crate::parse(
            r#"
            process select(mode: enum["a"] | dict) { finish mode }
            start select(mode: "nope")
            "#,
        )
        .expect("parse union expected type");

        assert!(matches!(
            LinkedModule::link(program, full_host_environment()),
            Err(LinkError::IncompatibleExpectedLiteral { .. })
        ));
    }

    #[test]
    fn branch_assignments_join_to_a_union_instead_of_first_wins() {
        let program = crate::parse(
            r#"
            process choose(flag: bool) {
              value = "initial"
              if flag { value = "text" } else { value = 1 }
              await tools.accept_str(value)?
            }
            "#,
        )
        .expect("parse branch join");

        assert!(matches!(
            LinkedModule::link(program, full_host_environment()),
            Err(LinkError::IncompatibleOperationInput { actual, .. }) if actual.contains('|')
        ));
    }

    #[test]
    fn for_bindings_use_list_elements_and_unknown_iterables_remain_gradual() {
        let known = crate::parse(
            r#"
            process consume() {
              for item in ["one", "two"] {
                await tools.accept_int(item)?
              }
            }
            "#,
        )
        .expect("parse known iterable");
        assert!(matches!(
            LinkedModule::link(known, full_host_environment()),
            Err(LinkError::IncompatibleOperationInput { .. })
        ));

        let unknown = crate::parse(
            r#"
            process consume(items: any) {
              for item in items {
                await tools.accept_int(item)?
              }
            }
            "#,
        )
        .expect("parse unknown iterable");
        LinkedModule::link(unknown, full_host_environment())
            .expect("unknown iterable elements should remain gradual");

        let non_list = crate::parse(
            r#"
            process consume() {
              for item in "not a list" { seen = item }
            }
            "#,
        )
        .expect("parse non-list iterable");
        assert!(matches!(
            LinkedModule::link(non_list, full_host_environment()),
            Err(LinkError::IncompatibleIterationTarget { .. })
        ));
    }

    #[test]
    fn awaited_process_handles_carry_the_inferred_process_output() {
        let program = crate::parse(
            r#"
            process child() { finish { value: "text" } }
            result = await start child()
            await tools.accept_int(result.value)?
            "#,
        )
        .expect("parse awaited process output");

        assert!(matches!(
            LinkedModule::link(program, full_host_environment()),
            Err(LinkError::IncompatibleOperationInput { actual, .. }) if actual.contains("str")
        ));
    }

    #[test]
    fn field_assignments_update_the_tracked_object_field_type() {
        let program = crate::parse(
            r#"
            state = { value: "text" }
            state.value = 1
            await tools.accept_str(state.value)?
            "#,
        )
        .expect("parse state mutation");

        assert!(matches!(
            LinkedModule::link(program, full_host_environment()),
            Err(LinkError::IncompatibleOperationInput { actual, .. }) if actual == "int"
        ));
    }

    #[test]
    fn missing_known_object_fields_are_errors_but_open_shapes_stay_gradual() {
        let known = crate::parse("value = { present: 1 }\nfinish value.missing")
            .expect("parse known object field access");
        assert!(matches!(
            LinkedModule::link(known, full_host_environment()),
            Err(LinkError::UnknownObjectField { field, .. }) if field == "missing"
        ));

        let open = crate::parse(
            r#"
            process inspect(map: dict, unknown: any) {
              finish [map.missing, unknown.missing]
            }
            "#,
        )
        .expect("parse open shape field access");
        LinkedModule::link(open, full_host_environment())
            .expect("dict and any field access should stay gradual");
    }

    #[test]
    fn union_field_assignments_update_matching_members_and_reject_unknown_fields() {
        let matching = crate::parse(
            r#"
            process mutate(flag: bool) {
              value = { a: 0 }
              if flag { value = { a: 0 } } else { value = { b: 0 } }
              value.a = 1
            }
            "#,
        )
        .expect("parse union field assignment");
        LinkedModule::link(matching, full_host_environment())
            .expect("a field present on one union member should remain assignable");

        let missing = crate::parse(
            r#"
            process mutate(flag: bool) {
              value = { a: 0 }
              if flag { value = { a: 0 } } else { value = { b: 0 } }
              value.c = 1
            }
            "#,
        )
        .expect("parse missing union field assignment");
        assert!(matches!(
            LinkedModule::link(missing, full_host_environment()),
            Err(LinkError::UnknownObjectField { field, .. }) if field == "c"
        ));
    }

    #[test]
    fn binary_operators_reject_known_category_errors_but_admit_unknown_maps() {
        let known = crate::parse("finish {} + 1").expect("parse bad binary operands");
        assert!(matches!(
            LinkedModule::link(known, full_host_environment()),
            Err(LinkError::IncompatibleBinaryOperands { .. })
        ));

        let gradual = crate::parse(
            r#"
            process combine(map: dict, unknown: any) {
              left = map + 1
              finish left + unknown
            }
            "#,
        )
        .expect("parse gradual binary operands");
        LinkedModule::link(gradual, full_host_environment())
            .expect("dict and any operands should stay gradual");
    }

    #[test]
    fn equality_accepts_a_compatible_union_member_but_rejects_known_category_mismatches() {
        let union = crate::parse(
            r#"
            process compare(flag: bool, number: int) {
              value = "initial"
              if flag { value = number } else { value = "text" }
              equal = value == "text"
              not_equal = "text" != value
              finish [equal, not_equal]
            }
            "#,
        )
        .expect("parse union equality");
        LinkedModule::link(union, full_host_environment())
            .expect("equality should accept a category-compatible union member");

        let incompatible = crate::parse("finish {} == 1").expect("parse incompatible equality");
        assert!(matches!(
            LinkedModule::link(incompatible, full_host_environment()),
            Err(LinkError::IncompatibleBinaryOperands { .. })
        ));
    }

    #[test]
    fn loop_carried_mutation_is_widened_after_one_forward_pass() {
        let program = crate::parse(
            r#"
            state = { value: "before" }
            while true {
              state.value = 1
            }
            await tools.accept_float(state.value)?
            "#,
        )
        .expect("parse loop-carried mutation");

        assert!(matches!(
            LinkedModule::link(program, full_host_environment()),
            Err(LinkError::IncompatibleOperationInput { actual, .. }) if actual.contains("str") && actual.contains("int")
        ));
    }

    fn typed_output_host_environment() -> LashlangHostEnvironment {
        let input_ty = TypeExpr::Object(vec![
            TypeField {
                name: "task".into(),
                ty: TypeExpr::Str,
                optional: false,
            },
            TypeField {
                name: "output".into(),
                ty: TypeExpr::Any,
                optional: true,
            },
        ]);
        let mut resources = LashlangHostCatalog::new();
        for (module, authority, default_schema) in [
            ("agents", "Agents", None),
            ("llm", "Llm", Some(TypeExpr::Str)),
        ] {
            resources
                .add_module_operation_binding(
                    [module],
                    authority,
                    if module == "agents" { "spawn" } else { "query" },
                    format!("{module}_typed_output"),
                    ResourceOperationBinding {
                        input_ty: input_ty.clone(),
                        output_ty: TypeExpr::Any,
                        output_from_input: Some(OutputFromInputBinding {
                            input_field: "output".to_string(),
                            default_schema,
                        }),
                    },
                )
                .expect("host catalog operation must not conflict");
        }
        resources
            .add_module_operation(
                ["static_tool"],
                "StaticTool",
                "run",
                "static_run",
                TypeExpr::Any,
                TypeExpr::Object(vec![TypeField {
                    name: "declared".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                }]),
            )
            .expect("host catalog operation must not conflict");
        LashlangHostEnvironment::new(resources, LashlangAbilities::all())
    }

    #[test]
    fn closed_type_literals_type_outputs_in_lowering_and_validation() {
        let direct = crate::parse(
            r#"
            result = (await agents.spawn({ task: "inspect", output: Type { declared: str } }))?
            result.declared
            "#,
        )
        .expect("parse direct typed output");
        LinkedModule::link(direct, typed_output_host_environment())
            .expect("declared output field should link");

        let missing_in_lowering = crate::parse(
            r#"
            result = (await agents.spawn({ task: "inspect", output: Type { declared: str } }))?
            result.undeclared
            "#,
        )
        .expect("parse missing direct output field");
        assert!(matches!(
            LinkedModule::link(missing_in_lowering, typed_output_host_environment()),
            Err(LinkError::UnknownObjectField { field, .. }) if field == "undeclared"
        ));

        let missing_in_validation = crate::parse(
            r#"
            process ask(llm: Llm) {
              result = (await llm.query({ task: "inspect", output: Type { declared: str } }))?
              finish result.undeclared
            }
            "#,
        )
        .expect("parse process typed output");
        assert!(matches!(
            LinkedModule::link(missing_in_validation, typed_output_host_environment()),
            Err(LinkError::UnknownObjectField { field, .. }) if field == "undeclared"
        ));
    }

    #[test]
    fn literal_type_defensively_degrades_type_literals_to_any() {
        assert_eq!(
            literal_type(&Expr::TypeLiteral(Box::new(TypeExpr::Str))),
            TypeExpr::Any
        );
    }

    #[test]
    fn declared_aliases_make_nested_type_literal_witnesses_closed() {
        let program = crate::parse(
            r#"
            type Inner = { value: str }
            result = (await agents.spawn({ task: "inspect", output: Type { nested: Inner } }))?
            result.nested.value
            "#,
        )
        .expect("parse aliased typed output");
        LinkedModule::link(program, typed_output_host_environment())
            .expect("declared aliases should close a schema witness");
    }

    #[test]
    fn record_shorthand_types_outputs_and_rejects_missing_fields() {
        let declared = crate::parse(
            r#"
            result = (await agents.spawn({
              task: "inspect",
              output: { declared: "str", count: "int", tags: "list[str]" }
            }))?
            values = [result.declared, result.count, result.tags]
            "#,
        )
        .expect("parse shorthand output");
        LinkedModule::link(declared, typed_output_host_environment())
            .expect("record shorthand fields should link");

        let missing = crate::parse(
            r#"
            result = (await llm.query({ task: "inspect", output: { declared: "str" } }))?
            result.undeclared
            "#,
        )
        .expect("parse shorthand missing field");
        assert!(matches!(
            LinkedModule::link(missing, typed_output_host_environment()),
            Err(LinkError::UnknownObjectField { field, .. }) if field == "undeclared"
        ));
    }

    #[test]
    fn dynamic_and_stored_schema_witnesses_stay_any() {
        for source in [
            r#"
            shape = Type { declared: str }
            result = (await agents.spawn({ task: "inspect", output: shape }))?
            result.undeclared
            "#,
            r#"
            inner = Type { value: str }
            result = (await agents.spawn({
              task: "inspect",
              output: Type { nested: inner }
            }))?
            result.undeclared
            "#,
        ] {
            let program = crate::parse(source).expect("parse dynamic schema witness");
            LinkedModule::link(program, typed_output_host_environment())
                .expect("dynamic schema witnesses must stay gradual");
        }
    }

    #[test]
    fn missing_witness_uses_default_schema_and_static_tools_are_unchanged() {
        let default_matches = crate::parse(
            r#"
            process ask(llm: Llm) -> str {
              finish (await llm.query({ task: "plain text" }))?
            }
            "#,
        )
        .expect("parse default output");
        LinkedModule::link(default_matches, typed_output_host_environment())
            .expect("llm query should default to str");

        let default_mismatch = crate::parse(
            r#"
            process ask(llm: Llm) -> int {
              finish (await llm.query({ task: "plain text" }))?
            }
            "#,
        )
        .expect("parse mismatched default output");
        assert!(matches!(
            LinkedModule::link(default_mismatch, typed_output_host_environment()),
            Err(LinkError::IncompatibleProcessReturn { expected, actual, .. })
                if expected == "int" && actual == "str"
        ));

        let static_output = crate::parse(
            r#"
            result = (await static_tool.run({}))?
            result.declared
            "#,
        )
        .expect("parse static output");
        LinkedModule::link(static_output, typed_output_host_environment())
            .expect("static P2 output type should be preserved");

        let static_missing = crate::parse(
            r#"
            result = (await static_tool.run({}))?
            result.undeclared
            "#,
        )
        .expect("parse static missing output field");
        assert!(matches!(
            LinkedModule::link(static_missing, typed_output_host_environment()),
            Err(LinkError::UnknownObjectField { field, .. }) if field == "undeclared"
        ));
    }

    #[tokio::test]
    async fn module_artifact_store_bytes_reject_corruption() {
        use crate::LashlangArtifactStore;

        let linked = LinkedModule::link(
            crate::parse("process scan() { finish 1 }").expect("parse module"),
            full_host_environment(),
        )
        .expect("link module");
        let store = crate::InMemoryLashlangArtifactStore::new();

        store
            .put_module_artifact(&linked.artifact)
            .await
            .expect("put artifact");
        assert_eq!(
            store
                .get_module_artifact(&linked.module_ref)
                .await
                .expect("get artifact")
                .expect("artifact exists")
                .module_ref,
            linked.module_ref
        );

        assert!(ModuleArtifact::from_store_bytes(b"not json").is_err());
    }

    #[test]
    fn shaping_builtins_link_valid_shapes_and_reject_every_known_wrong_shape() {
        let valid = crate::parse(
            r#"
            finish {
              sorted: sort([2, 1]),
              sorted_by: sort_by([{ rank: 2 }, { rank: 1 }], "rank"),
              total: sum([1, 2]),
              least: min([1, 2]),
              greatest: max([1, 2]),
              rewritten: replace("aba", "a", "x"),
              lower: lower("ABC"),
              upper: upper("abc"),
              unique: unique([1, 1]),
              reversed: reverse([1, 2])
            }
            "#,
        )
        .expect("parse valid shaping builtins");
        LinkedModule::link(valid, full_host_environment())
            .expect("valid shaping builtin types should link");

        let invalid = [
            ("sort", r#"finish sort([1, "two"])"#),
            ("sort_by", r#"finish sort_by([1], "rank")"#),
            ("sum", r#"finish sum([1, "two"])"#),
            ("min", "finish min([[1], [2]])"),
            ("max", "finish max({ value: 1 })"),
            ("replace", r#"finish replace("a", "a", 1)"#),
            ("lower", "finish lower(1)"),
            ("upper", "finish upper(false)"),
            ("unique", "finish unique(1)"),
            ("reverse", "finish reverse({ value: 1 })"),
        ];
        for (builtin, source) in invalid {
            let program = crate::parse(source).expect("parse invalid shaping builtin");
            assert!(
                matches!(
                    LinkedModule::link(program, full_host_environment()),
                    Err(LinkError::IncompatibleBuiltinOperands { builtin: actual, .. })
                        if actual == builtin
                ),
                "expected `{builtin}` typing failure for {source}"
            );
        }

        let dict_items = crate::parse("process shape(items: dict) { finish reverse(items) }")
            .expect("parse dict shaping input");
        assert!(matches!(
            LinkedModule::link(dict_items, full_host_environment()),
            Err(LinkError::IncompatibleBuiltinOperands { builtin, .. }) if builtin == "reverse"
        ));
    }
}
