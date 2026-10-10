//! What a build makes of the helper releases it holds (FIG-5799).
//!
//! A release is data (`releases.rs`): every function a build of it holds,
//! by identity, with its definition and, for a native implementation, the
//! digest of what the code behind it answers (`probe.rs`). The build
//! resolves its declarations (`declared.rs`): each sealed release as the
//! repository keeps it, and the release the tree builds, until the cut seals
//! it, as the build defines it (FIG-5839). It then checks each definition's
//! identity, holds every body a run may still pin that the build does not
//! define itself, and compares each native digest with the build's own: a
//! native that answers otherwise under the same identity fails the build,
//! since its code is not in its definition and it is stated anew under its
//! next native version (`K-LIB-011`). Any other difference from a sealed
//! release fails the build too.
//!
//! The build script includes this file.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{FunctionDefinition, FunctionId, FunctionRegistry};

use crate::releases::{HeldReleases, HelperRelease, ReleasedFunction};

/// The releases `retained` and `retiring` declare, oldest first: each
/// sealed one as `sealed` keeps it, and the release the tree builds, the
/// last retained, as `building` defines it after the release before it
/// while no sealed release is kept for it. Every sealed release is
/// declared, and only the release the tree builds may be unsealed.
///
/// # Errors
///
/// A sealed release that does not decode, is not sealed or is not
/// declared, and a declaration no release answers.
pub(crate) fn resolve(
    retained: &[(&str, u32)],
    retiring: &[(&str, u32)],
    sealed: &[&str],
    building: impl FnOnce(&str, u32, Option<&HelperRelease>) -> HelperRelease,
) -> Result<HeldReleases, String> {
    let mut kept = BTreeMap::new();
    for text in sealed {
        let release = HelperRelease::decode(text)?;
        if !release.sealed {
            return Err(format!(
                "helper release {} is kept as shipped, and is not sealed",
                release.release
            ));
        }
        if let Some(twice) = kept.insert(release.release.clone(), release) {
            return Err(format!("helper release {} is kept twice", twice.release));
        }
    }
    let mut take = |name: &str, ordinal: u32| match kept.remove(name) {
        Some(release) if release.ordinal != ordinal => Err(format!(
            "helper release {name} is kept as ordinal {}, and declared as {ordinal}",
            release.ordinal
        )),
        release => Ok(release),
    };
    let Some(((name, ordinal), shipped)) = retained.split_last() else {
        return Err("no helper release is retained".into());
    };
    let mut held = HeldReleases::default();
    for (earlier, ordinal) in shipped {
        let release = take(earlier, *ordinal)?.ok_or_else(|| {
            format!(
                "helper release {earlier} is retained before the release the tree builds, \
                 and no sealed release is kept for it"
            )
        })?;
        held.retained.push(release);
    }
    let release = match take(name, *ordinal)? {
        Some(release) => release,
        None => building(name, *ordinal, held.retained.last()),
    };
    held.retained.push(release);
    for (name, ordinal) in retiring {
        let release = take(name, *ordinal)?.ok_or_else(|| {
            format!("retiring helper release {name} has no sealed release kept for it")
        })?;
        held.retiring.push(release);
    }
    if let Some(name) = kept.keys().next() {
        return Err(format!(
            "sealed helper release {name} is declared neither retained nor retiring"
        ));
    }
    Ok(held)
}

/// The functions of `releases` that `registry`, the build's own, does not
/// hold, each after the functions its body calls, checked against it:
/// every definition under its identity, every native the release holds
/// implemented here and answering as it did, and every body's callees held.
///
/// # Errors
///
/// Any difference: the release the tree builds is the build's own until it
/// is sealed, so only a sealed release can differ, and a build that cannot
/// hold a sealed release exactly fails.
pub(crate) fn keep(
    releases: &[HelperRelease],
    registry: &FunctionRegistry,
    fingerprints: &BTreeMap<FunctionId, String>,
) -> Result<Vec<FunctionDefinition>, String> {
    let mut kept = Vec::new();
    let mut held: BTreeSet<FunctionId> = registry.iter().map(|(function, _)| *function).collect();
    for (position, release) in releases.iter().enumerate() {
        let differs =
            |message: String| Err(format!("helper release {}: {message}", release.release));
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
                    return differs(format!(
                        "native `{name}` ({function}) answers otherwise than when the release was \
                         sealed; a changed native implementation is stated under its next native \
                         version (`native 2`), which is a new function"
                    ));
                }
                continue;
            }
            if held.contains(&function) {
                continue;
            }
            if released.definition.has_native() {
                return differs(format!(
                    "native `{name}` ({function}) is not implemented by this build"
                ));
            }
            let calls = released
                .definition
                .body()
                .map(|body| body.functions.keys().copied().collect::<Vec<_>>())
                .unwrap_or_default();
            if let Some(missing) = calls.iter().find(|called| !held.contains(called)) {
                return differs(format!(
                    "`{name}` ({function}) calls {missing}, which this build does not hold"
                ));
            }
            held.insert(function);
            kept.push(released.definition.clone());
        }
        if position + 1 == releases.len() {
            let own: BTreeSet<FunctionId> =
                registry.iter().map(|(function, _)| *function).collect();
            let added = own.difference(&release.writes).count();
            let dropped = release.writes.difference(&own).count();
            if added + dropped > 0 {
                return differs(format!(
                    "the build's functions are not the ones the release writes ({added} not in it, \
                     {dropped} of it not built); a build changes a shipped helper by declaring the \
                     next release (`RETAINED_HELPER_RELEASES`)"
                ));
            }
        }
    }
    Ok(kept)
}

/// Release `release`, ordinal `ordinal`, as the build defines it from
/// `registry`, its own functions: it writes them. Only a sealed `previous`
/// protects runnable functions a shipped run can pin.
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
