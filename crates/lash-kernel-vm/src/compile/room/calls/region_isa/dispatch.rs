//! All source/Link placement work is cold. Invocation borrows its lookup key.
use super::*;
use std::collections::BTreeMap;

#[derive(Clone, PartialEq, Eq)]
struct ClosureEntry {
    site: Site,
    names: Box<[lash_kernel_value::Name]>,
    target: u32,
}
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Dispatch {
    functions: BTreeMap<lash_kernel_value::FunctionId, u32>,
    closures:
        BTreeMap<(lash_kernel_value::LinkId, lash_kernel_value::FunctionId), Box<[ClosureEntry]>>,
}
impl Dispatch {
    pub(super) fn compile(exe: &Executable, targets: &[CalleePlan]) -> Result<Self, Refused> {
        let by_code: BTreeMap<_, _> = targets
            .iter()
            .enumerate()
            .map(|(index, target)| {
                Ok((
                    target.code(),
                    u32::try_from(index).map_err(|_| Refused::Path)?,
                ))
            })
            .collect::<Result<_, Refused>>()?;
        let mut functions = BTreeMap::new();
        for (index, target) in targets.iter().enumerate() {
            if exe.code(target.code()).created.is_some() {
                continue;
            }
            if let doc::constant_pool::PoolOwner::Function(function) = target.source().owner {
                functions.insert(function, u32::try_from(index).map_err(|_| Refused::Path)?);
            }
        }
        let mut closures: BTreeMap<_, Vec<ClosureEntry>> = BTreeMap::new();
        for (function, site, names, code) in exe.closure_entries() {
            let Some(&target) = by_code.get(&code) else {
                continue;
            };
            for &link in exe.instances.keys() {
                // This canonical lookup performs origin and reachability
                // checks once, while compiling, including same-site siblings.
                if exe.closure_at(function, link, site, names) != Some(code) {
                    continue;
                }
                closures
                    .entry((link, *function))
                    .or_default()
                    .push(ClosureEntry {
                        site: site.clone(),
                        names: names.into(),
                        target,
                    });
            }
        }
        let closures = closures
            .into_iter()
            .map(|(key, mut entries)| {
                entries.sort_by(|a, b| a.site.cmp(&b.site).then_with(|| a.names.cmp(&b.names)));
                entries.dedup_by(|a, b| {
                    a.site == b.site && a.names == b.names && a.target == b.target
                });
                (key, entries.into_boxed_slice())
            })
            .collect();
        Ok(Self {
            functions,
            closures,
        })
    }
    pub(super) fn target(
        &self,
        exe: &Executable,
        identity: &lash_kernel_value::CallableId,
        closure: Option<&doc::arena::ClosureIdentity>,
    ) -> Option<usize> {
        if !exe.admits_callable(identity) {
            return None;
        }
        match closure {
            None => self
                .functions
                .get(&identity.function)
                .map(|index| *index as usize),
            Some(closure) => {
                if &closure.callable != identity {
                    return None;
                }
                let entries = self.closures.get(&(identity.link, identity.function))?;
                let index = entries
                    .binary_search_by(|entry| {
                        entry
                            .site
                            .cmp(&closure.site.site)
                            .then_with(|| entry.names.as_ref().cmp(&closure.capture_names))
                    })
                    .ok()?;
                Some(entries[index].target as usize)
            }
        }
    }
}
