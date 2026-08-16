pub fn benchmark_host_environment() -> &'static LashlangHostEnvironment {
    static SURFACE: OnceLock<LashlangHostEnvironment> = OnceLock::new();
    SURFACE.get_or_init(build_benchmark_host_environment)
}

fn build_benchmark_host_environment() -> LashlangHostEnvironment {
    let mut resources = LashlangHostCatalog::tool_default(["echo", "boom", "missing_tool"]);
    lashlang::add_trigger_resource_operations(&mut resources);
    resources
        .add_trigger_source_constructor(
            ["cron", "Schedule"],
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
            lashlang::NamedDataType::object(
                "cron.Tick",
                vec![TypeField {
                    name: "fired_at".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                }],
            )
            .expect("valid cron tick type"),
        )
        .expect("valid cron trigger source");
    resources
        .add_trigger_source_constructor(
            ["ui", "button", "pressed"],
            TypeExpr::Object(Vec::new()),
            lashlang::NamedDataType::object(
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
    resources
        .add_module_operation(
            ["shell"],
            "Shell",
            "exec",
            "exec_command",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["llm"],
            "Llm",
            "query",
            "llm_query",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["agents"],
            "Agents",
            "spawn",
            "spawn_agent",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["processes"],
            "Processes",
            "list",
            "list_process_handles",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["control"],
            "Control",
            "continue_as",
            "continue_as",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    LashlangHostEnvironment::new(resources, LashlangAbilities::all())
        .with_globals(["history", "ctx", "snap", "img", "docs", "proj"])
}

pub fn linked_benchmark_program(source: &str) -> LinkedModule {
    LinkedModule::link(
        lashlang::parse(source).expect("benchmark program should parse"),
        benchmark_host_environment(),
    )
    .expect("benchmark program should link")
}

pub fn projected_bindings(scenario: Scenario) -> ProjectedBindings {
    let mut bindings = ProjectedBindings::new();
    if !matches!(
        scenario,
        Scenario::ProjectedValues
            | Scenario::ProjectedOperations
            | Scenario::ContinueAsSeedHostEnvironment
    ) {
        return bindings;
    }
    match scenario {
        Scenario::ProjectedValues => {
            bindings.insert(
                "history",
                ProjectedValue::custom("history", Arc::new(ProjectedList::history())),
            );
            bindings.insert(
                "docs",
                ProjectedValue::scalar("docs", projected_docs_record()),
            );
        }
        Scenario::ProjectedOperations | Scenario::ContinueAsSeedHostEnvironment => {
            bindings.insert(
                "proj",
                ProjectedValue::scalar("proj", projected_operations_record()),
            );
        }
        _ => {}
    }
    bindings
}
