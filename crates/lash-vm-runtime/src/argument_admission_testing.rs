//! Argument-admission laws over a host's module artifact store.
#![expect(clippy::expect_used, reason = "conformance fixture assertions")]
use super::*;
use lash_vm::testing::ast_builders as b;

fn process_module(
    name: &str,
    params: Vec<lash_vm::ProcessParam>,
    output: lash_vm::TypeExpr,
    body: lash_vm::Expr,
) -> lash_vm::Program {
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
async fn compile_fixture(
    program: lash_vm::Program,
    environment: &LashVmHostEnvironment,
) -> lash_vm_client::service::CompiledModule {
    match lash_vm_client::service::Service::default()
        .request_accounted(lash_vm_client::service::Request::LinkAst {
            program,
            environment: environment.clone(),
        })
        .await
        .expect("compile fixture in worker")
    {
        lash_vm_client::service::Response::Module(module) => *module,
        response => panic!("worker refused fixture: {response:?}"),
    }
}
pub async fn nested_process_arguments_reject_forged_aliases_and_try_later_union_arms(
    store: Arc<dyn lash_core::ModuleArtifactStore>,
) {
    let store = LashVmArtifacts::new(store);
    let environment = LashVmHostEnvironment::default();
    let handler = compile_fixture(
        process_module(
            "handler",
            vec![
                b::param("event", lash_vm::TypeExpr::Str),
                b::param("other", lash_vm::TypeExpr::Str),
            ],
            lash_vm::TypeExpr::Bool,
            b::bool_lit(true),
        ),
        &environment,
    )
    .await;
    let process_type = b::process_type(
        vec![
            b::param("event", lash_vm::TypeExpr::Str),
            b::param("other", lash_vm::TypeExpr::Str),
        ],
        lash_vm::TypeExpr::Bool,
    );
    let receiver = compile_fixture(
        process_module(
            "install",
            vec![b::param(
                "envelope",
                lash_vm::TypeExpr::Object(vec![b::type_field(
                    "handlers",
                    lash_vm::TypeExpr::List(Box::new(lash_vm::TypeExpr::union(vec![
                        process_type,
                        lash_vm::TypeExpr::Str,
                    ]))),
                    false,
                )]),
            )],
            lash_vm::TypeExpr::Bool,
            b::bool_lit(true),
        ),
        &environment,
    )
    .await;
    for artifact in [&handler.artifact, &receiver.artifact] {
        store
            .publish_module_artifact(&host_claim(), artifact)
            .await
            .expect("publish artifact");
    }
    let valid = handler
        .artifact
        .definition_identity("handler")
        .expect("handler identity")
        .to_process_value();
    let start = |value| {
        let mut args = lash_vm::Record::new();
        args.insert(
            "envelope".into(),
            lash_vm::from_json(
                serde_json::json!({"handlers":[value,"later nonprocess union arm"]}),
            ),
        );
        lash_vm::ProcessStart {
            module_ref: receiver.module_ref.clone(),
            process_ref: receiver
                .artifact
                .process_ref("install")
                .expect("receiver export")
                .clone(),
            host_requirements_ref: receiver.host_requirements_ref.clone(),
            start_site: lash_vm::LashVmExecutionCallSite {
                at: lash_sansio::WorkflowOccurrence::new(
                    lash_sansio::WorkflowSiteRef::node(lash_vm::workflow_node_id(
                        "process:install",
                        &[],
                    )),
                    std::num::NonZeroU64::MIN,
                ),
                kind: lash_sansio::ExecutionNodeKind::Call,
                label: "nested start".into(),
            },
            process_name: "install".into(),
            args,
        }
    };
    let artifacts: LashVmArtifacts = store;
    prepare_lash_vm_process_start(
        &lash_vm_client::service::Service::default(),
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
        let error = prepare_lash_vm_process_start(
            &lash_vm_client::service::Service::default(),
            artifacts.clone(),
            None,
            start(forged),
            lash_core::ProcessOriginator::host(),
            lash_core::Lifetime::Detached.into(),
        )
        .await
        .expect_err("forged nested alias refuses before a start request exists");
        assert!(
            matches!(error, LashVmRuntimeError::InvalidProcessArgument { ref path, .. } if path == "envelope.handlers[0]"),
            "{error:?}"
        );
    }
}
