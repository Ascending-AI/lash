//! What a build makes of the helper releases it retains (FIG-5799).
//!
//! A release is frozen as data (`lash-vm-library`): every function a build
//! of it holds, by identity, with its definition and, for a native
//! implementation, the digest of what the code behind it answers
//! (`probe.rs`). The build checks each definition's identity, holds every
//! body a run may still pin that the build does not define itself, and
//! compares each native digest with the build's own: a native that answers
//! otherwise under the same identity fails the build, since its code is not
//! in its definition and it is stated anew under its next native version
//! (`K-LIB-011`). A sealed release, one that has shipped, fails the build on
//! any other difference; the release the tree is still building records it
//! for its law, which names the generator that freezes it again.
//!
//! The build script includes this file.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{FunctionDefinition, FunctionId, FunctionRegistry};
use lash_vm_library::{FREEZE, HelperRelease, ReleasedFunction};

/// What a build holds of the releases it retains.
#[derive(Debug, Default)]
pub(crate) struct Kept {
    /// The functions the build holds besides its own, each after the
    /// functions its body calls.
    pub(crate) retained: Vec<FunctionDefinition>,
    /// Where the release the tree is still building and the build part.
    pub(crate) divergence: Vec<String>,
}

/// The functions of `releases` that `registry`, the build's own, does not
/// hold, checked against it: every definition under its identity, every
/// native the release holds implemented here and answering as it did.
///
/// # Errors
///
/// A definition that is not under its identity, and any difference from a
/// sealed release.
pub(crate) fn keep(
    releases: &[HelperRelease],
    registry: &FunctionRegistry,
    fingerprints: &BTreeMap<FunctionId, String>,
) -> Result<Kept, String> {
    let mut kept = Kept::default();
    let mut held: BTreeSet<FunctionId> = registry.iter().map(|(function, _)| *function).collect();
    for release in releases {
        let mut differs = |message: String| {
            let message = format!("helper release {}: {message}", release.release);
            if release.sealed {
                Err(message)
            } else {
                kept.divergence.push(message);
                Ok(())
            }
        };
        let listed: BTreeSet<FunctionId> = release
            .functions
            .iter()
            .map(|function| function.function)
            .collect();
        if let Some(missing) = release.writes.iter().find(|id| !listed.contains(id)) {
            return Err(format!(
                "helper release {} writes {missing}, which it does not hold",
                release.release
            ));
        }
        for released in &release.functions {
            let function = released.function;
            let name = &released.definition.name;
            let identity = released
                .definition
                .identity()
                .map_err(|error| error.to_string())?;
            if identity != function {
                return Err(format!(
                    "helper release {} holds `{name}` as {function}, and its definition is {identity}",
                    release.release
                ));
            }
            if let Some(registered) = registry.get(&function) {
                // A native that answers otherwise under the identity a run
                // pinned would change that run: the build fails, whatever
                // the release (`K-LIB-011`).
                if registered.native.is_some()
                    && released.fingerprint.as_ref() != fingerprints.get(&function)
                {
                    return Err(format!(
                        "helper release {}: native `{name}` ({function}) answers otherwise than when the \
                         release was frozen; a changed native implementation is stated under its next \
                         native version (`native 2`), which is a new function, and the release is frozen \
                         again: {FREEZE}",
                        release.release
                    ));
                }
                continue;
            }
            if held.contains(&function) {
                continue;
            }
            if released.definition.has_native() {
                differs(format!(
                    "native `{name}` ({function}) is not implemented by this build"
                ))?;
                continue;
            }
            let calls = released
                .definition
                .body()
                .map(|body| body.functions.keys().copied().collect::<Vec<_>>())
                .unwrap_or_default();
            if let Some(missing) = calls.iter().find(|called| !held.contains(called)) {
                differs(format!(
                    "`{name}` ({function}) calls {missing}, which this build does not hold"
                ))?;
                continue;
            }
            held.insert(function);
            kept.retained.push(released.definition.clone());
        }
        if !release.sealed {
            let own: BTreeSet<FunctionId> =
                registry.iter().map(|(function, _)| *function).collect();
            let added = own.difference(&release.writes).count();
            let dropped = release.writes.difference(&own).count();
            if added + dropped > 0 {
                differs(format!(
                    "the build's functions are not the ones the release writes \
                     ({added} not in it, {dropped} of it not built); freeze it again: {FREEZE}"
                ))?;
            }
        }
    }
    Ok(kept)
}

/// Release `release`, ordinal `ordinal`, frozen for `registry`, the build's
/// own functions: it writes them. Only a sealed `previous` protects runnable
/// functions a shipped run can pin; an unsealed baseline is replaced.
pub(crate) fn freeze(
    release: &str,
    ordinal: u32,
    previous: Option<&HelperRelease>,
    registry: &FunctionRegistry,
    fingerprints: &BTreeMap<FunctionId, String>,
) -> HelperRelease {
    let mut functions = Vec::new();
    let mut listed = BTreeSet::new();
    for (function, _) in registry.iter() {
        list(
            *function,
            registry,
            fingerprints,
            &mut listed,
            &mut functions,
        );
    }
    let writes = listed.clone();
    for released in previous
        .filter(|previous| previous.sealed)
        .map_or(&[][..], |previous| &previous.functions)
    {
        let runnable = !released.definition.has_native()
            && released
                .definition
                .body()
                .is_some_and(|body| body.functions.keys().all(|called| listed.contains(called)));
        if runnable && listed.insert(released.function) {
            functions.push(ReleasedFunction {
                fingerprint: None,
                ..released.clone()
            });
        }
    }
    HelperRelease {
        release: release.to_owned(),
        ordinal,
        sealed: false,
        writes,
        functions,
    }
}

/// Lists `function` after every function its body calls.
fn list(
    function: FunctionId,
    registry: &FunctionRegistry,
    fingerprints: &BTreeMap<FunctionId, String>,
    listed: &mut BTreeSet<FunctionId>,
    functions: &mut Vec<ReleasedFunction>,
) {
    if listed.contains(&function) {
        return;
    }
    let Some(registered) = registry.get(&function) else {
        return;
    };
    if let Some(body) = registered.definition.body() {
        for called in body.functions.keys() {
            list(*called, registry, fingerprints, listed, functions);
        }
    }
    listed.insert(function);
    functions.push(ReleasedFunction {
        function,
        definition: FunctionDefinition::clone(&registered.definition),
        fingerprint: fingerprints.get(&function).cloned(),
    });
}
