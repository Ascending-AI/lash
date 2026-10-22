//! Explicit process component namespace fixture.
use lash::package::*;
/// One explicitly installed component namespace and the host's all-package attachment.
pub async fn namespace(
    core: &lash::LashCore,
    pin: &lash_core::HostArtifactPin,
    env: &lash::process::ProcessEnv,
) -> (
    RecordedPackages,
    lash_vm_runtime::WorkflowEnvironmentRequest,
) {
    let context = PublicationContext {
        domain: PublicationDomain(core.backend().binding_identity().to_string()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("kernel-process-law".into()),
        root: Segment::new("law").unwrap(),
    };
    let packages = core
        .host_artifacts()
        .process_namespace(pin, env, context.clone())
        .await
        .unwrap();
    let request = lash_vm_runtime::WorkflowEnvironmentRequest {
        snapshot: packages.snapshot.clone(),
        context,
        attachment: Attachment {
            packages: packages
                .snapshot
                .advertisements
                .iter()
                .filter_map(|accepted| match &accepted.advertisement {
                    PackageAdvertisement::Enumerated(package) => Some(package.path.clone()),
                    _ => None,
                })
                .collect(),
            members: Default::default(),
            deferred: Default::default(),
        },
    };
    (packages, request)
}
