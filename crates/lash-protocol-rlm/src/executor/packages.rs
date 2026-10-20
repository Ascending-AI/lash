//! A cell's explicit package view, and OP-INTERNAL admission at its authored units.
use super::{CellServices, Refused, host_failure};
use lash_kernel_doc::IdentifiedDocument;
use lash_kernel_package::{HeldPackages, PackageView};
use lash_vm_runtime::HostBoundary;
use std::sync::Arc;

pub(super) fn held(services: &CellServices) -> Result<Arc<HeldPackages>, String> {
    services
        .workers
        .config()
        .packages
        .clone()
        .map_or_else(lash_vm_runtime::lash_packages, Ok)
        .map_err(|error| error.to_string())
}

pub(super) fn view(
    held: &HeldPackages,
    packages: crate::CellPackages,
) -> Result<PackageView<'_>, Refused> {
    held.view(&packages.digest()).map_err(|_| {
        lash_core::CellFailure::new(
            lash_core::CellFailureKind::PackagesNotHeld {
                packages: packages.digest(),
            },
            format!("this cell's package set {} is not held", packages.digest()),
        )
        .into()
    })
}

pub(super) fn admit(
    document: &IdentifiedDocument,
    boundary: &HostBoundary,
    held: &HeldPackages,
) -> Result<(), Refused> {
    let internal = |effect: &lash_kernel_doc::EffectName| {
        boundary.effect(effect).is_some_and(|(_, offered)| {
            offered.visibility == lash_kernel_dialect::Visibility::Internal
        })
    };
    let graph = lash_kernel_check::derive_view(document, held.functions().as_ref())
        .map_err(|error| host_failure(error.to_string()))?;
    let mut admission = lash_kernel_check::Environment::new(held.functions().as_ref());
    admission.internal = Box::new(internal);
    admission.check_visibility(&graph).map_err(|error| {
        let internal =
            error
                .errors
                .iter()
                .find_map(|refusal| match (&refusal.reason, &refusal.site) {
                    (
                        lash_kernel_check::RefusalReason::InternalOperation { effect },
                        Some(site),
                    ) => Some((effect.clone(), site.unit.clone())),
                    _ => None,
                });
        match internal {
            Some((operation, unit)) => {
                lash_core::CellFailure::new(lash_core::CellFailureKind::Policy, error.to_string())
                    .with_defect(lash_core::CellDefect::InternalOperation { operation, unit })
                    .into()
            }
            None => host_failure(error.to_string()),
        }
    })
}

pub(super) async fn select(
    state: &mut super::CodeModeExecutionState,
    services: &CellServices,
) -> Result<crate::CellPackages, Refused> {
    match &services.packages {
        Some(gate) => {
            let packages = match state.shown_packages() {
                Some(shown) => shown,
                None => gate
                    .writable()
                    .await
                    .map_err(|error| Refused::Packages(Box::new(error)))?,
            };
            Ok(packages)
        }
        None => Ok(crate::CellPackages(
            held(services).map_err(host_failure)?.own(),
        )),
    }
}

pub(super) fn prepare<'a>(
    held: &'a HeldPackages,
    packages: crate::CellPackages,
    boundary: &mut HostBoundary,
) -> Result<PackageView<'a>, Refused> {
    let view = view(held, packages)?;
    boundary.in_packages(&view);
    Ok(view)
}
