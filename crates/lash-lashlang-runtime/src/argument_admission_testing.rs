//! Argument-admission laws over a host's module artifact store.
#![expect(clippy::expect_used, reason = "conformance fixture assertions")]
use super::*;
use lashlang::testing::ast_builders as b;
fn process_module(
    name: &str,
    params: Vec<lashlang::ProcessParam>,
    output: lashlang::TypeExpr,
    body: lashlang::Expr,
) -> lashlang::Program {
    b::module(
        vec![b::process_returning(name, params, output, b::finish(body))],
        Vec::new(),
    )
}
fn host_claim() -> lash_core::ReferrerClaim {
    lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        lash_core::HostArtifactPin::mint(),
    ))
    .expect("unguarded host pin")
}
pub async fn nested_process_arguments_reject_forged_aliases_and_try_later_union_arms(
    store: Arc<dyn lash_core::ModuleArtifactStore>,
) {
    let store = LashlangArtifacts::new(store);
    let environment = LashlangHostEnvironment::default();
    let handler = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "nested handler",
        program: process_module(
            "handler",
            vec![
                b::param("event", lashlang::TypeExpr::Str),
                b::param("other", lashlang::TypeExpr::Str),
            ],
            lashlang::TypeExpr::Bool,
            b::bool_lit(true),
        ),
        environment: &environment,
    })
    .expect("compile immutable handler");
    let process_type = b::process_type(
        vec![
            b::param("event", lashlang::TypeExpr::Str),
            b::param("other", lashlang::TypeExpr::Str),
        ],
        lashlang::TypeExpr::Bool,
    );
    let receiver = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "nested union receiver",
        program: process_module(
            "install",
            vec![b::param(
                "envelope",
                lashlang::TypeExpr::Object(vec![b::type_field(
                    "handlers",
                    lashlang::TypeExpr::List(Box::new(lashlang::TypeExpr::union(vec![
                        process_type,
                        lashlang::TypeExpr::Str,
                    ]))),
                    false,
                )]),
            )],
            lashlang::TypeExpr::Bool,
            b::bool_lit(true),
        ),
        environment: &environment,
    })
    .expect("compile receiver");
    for artifact in [&handler.artifact, &receiver.artifact] {
        store
            .publish_module_artifact(&host_claim(), artifact)
            .await
            .expect("publish artifact");
    }
    let valid =
        lashlang::ProcessDefinitionIdentity::from_artifact_export(&handler.artifact, "handler")
            .expect("handler identity")
            .to_process_value();
    let start = |value| {
        let mut args = lashlang::Record::new();
        args.insert(
            "envelope".into(),
            lashlang::from_json(
                serde_json::json!({"handlers":[value,"later nonprocess union arm"]}),
            ),
        );
        lashlang::ProcessStart {
            module_ref: receiver.module_ref.clone(),
            process_ref: receiver
                .artifact
                .process_ref("install")
                .expect("receiver export")
                .clone(),
            host_requirements_ref: receiver.host_requirements_ref.clone(),
            start_site: lashlang::LashlangExecutionCallSite {
                site: lashlang::LashlangExecutionSite {
                    node_id: "nested-union".into(),
                    node_kind: lash_sansio::ExecutionNodeKind::Call,
                    label: "nested start".into(),
                    branch: None,
                    workflow_site: lashlang::WorkflowExecutionSite::new(
                        "process:install",
                        [],
                        lash_sansio::ExecutionNodeKind::Call,
                        "nested start",
                    ),
                },
                occurrence: 1,
            },
            process_name: "install".into(),
            args,
        }
    };
    let artifacts: LashlangArtifacts = store;
    prepare_lashlang_process_start(
        artifacts.clone(),
        None,
        start(valid.clone()),
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached.into(),
    )
    .await
    .expect("later union arm and honest nested identity pass");
    for field in ["process_name", "module_ref", "host_requirements_ref"] {
        let mut forged = valid.clone();
        forged[field] = serde_json::json!("forged-alias");
        let error = prepare_lashlang_process_start(
            artifacts.clone(),
            None,
            start(forged),
            lash_core::ProcessOriginator::host(),
            lash_core::Lifetime::Detached.into(),
        )
        .await
        .expect_err("forged nested alias refuses before a start request exists");
        assert!(
            matches!(error, LashlangRuntimeError::InvalidProcessArgument { ref path, .. } if path == "envelope.handlers[0]"),
            "{error:?}"
        );
    }
}
