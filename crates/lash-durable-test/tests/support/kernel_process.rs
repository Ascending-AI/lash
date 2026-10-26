//! Complete host-authored Documents linked against explicit process components.
use lash::package::*;

/// The caller authors final kernel text and names the components it installs.
/// Operation signatures come from those components' current declarations once;
/// the retained Link pins their contracts for execution and cold resume.
pub async fn publish(
    core: &lash::LashCore,
    text: &str,
    env: &lash::process::ProcessEnv,
) -> (serde_json::Value, lash::process::ProcessEnv) {
    let mut document = lash_kernel_doc::parse_document(text).expect("the final document parses");
    let artifacts = core.host_artifacts();
    let pin = lash_core::HostArtifactPin::mint();
    let (packages, environment) = namespace(core, &pin, env).await;
    for import in document.imports.values_mut() {
        if let lash_kernel_value::ImportKind::Operation { signature } = &mut import.kind {
            for accepted in &packages.snapshot.advertisements {
                if let PackageAdvertisement::Enumerated(package) = &accepted.advertisement
                    && package.path == import.name.package
                    && let Some(Member {
                        kind: MemberKind::Operation(offer),
                        ..
                    }) = package.members.get(&import.name.member)
                {
                    *signature = offer.requirement.signature.clone();
                }
            }
        }
    }
    let start = artifacts
        .publish_workflow(
            &pin,
            &lash_kernel_doc::DocumentBytes::from(document),
            &StartSelector::Main,
            &environment,
            None,
            None,
        )
        .await
        .unwrap();
    let env = artifacts
        .link_process_env(
            env,
            &lash::process::LinkTarget::Linked(start.clone()),
            &packages,
            &Default::default(),
        )
        .await
        .unwrap();
    let payload = serde_json::to_value(lash_vm_runtime::KernelProcessInput {
        code: lash_vm_runtime::process::InlineRequest::Linked { start },
        args: Default::default(),
    })
    .unwrap();
    (payload, env)
}

#[path = "kernel_namespace.rs"]
mod namespace;
pub use namespace::namespace;
