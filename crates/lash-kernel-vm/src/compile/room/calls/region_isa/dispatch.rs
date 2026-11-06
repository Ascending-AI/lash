//! A dynamic site's targets, compiled from source the first time a run
//! reaches each one. Later invocations borrow their lookup key.
use super::*;
use std::collections::BTreeMap;

struct ClosureEntry {
    site: Site,
    names: Box<[lash_kernel_value::Name]>,
    target: Option<Arc<CalleePlan>>,
}

#[derive(Default)]
pub(super) struct Dispatch {
    codes: BTreeMap<CodeId, Option<Arc<CalleePlan>>>,
    functions: BTreeMap<lash_kernel_value::FunctionId, Option<Arc<CalleePlan>>>,
    closures:
        BTreeMap<(lash_kernel_value::LinkId, lash_kernel_value::FunctionId), Vec<ClosureEntry>>,
}

impl Dispatch {
    /// The plan for `code` as the site's target, or None when the site
    /// does not admit it.
    pub(super) fn code(
        &mut self,
        exe: &Executable,
        caller: CodeId,
        site: &target::DynamicSite,
        code: CodeId,
    ) -> Option<Arc<CalleePlan>> {
        if let Some(known) = self.codes.get(&code) {
            return known.clone();
        }
        let plan = site
            .admit(exe, code)
            .and_then(|resolved| CalleePlan::compile(exe, caller, &resolved))
            .ok()
            .map(Arc::new);
        self.codes.insert(code, plan.clone());
        plan
    }

    /// The plan for an actual callable: a closure by its defining function,
    /// creation site and capture names, a function by its declared body.
    pub(super) fn callable(
        &mut self,
        exe: &Executable,
        caller: CodeId,
        site: &target::DynamicSite,
        identity: &lash_kernel_value::CallableId,
        closure: Option<&doc::arena::ClosureIdentity>,
    ) -> Option<Arc<CalleePlan>> {
        if !exe.admits_callable(identity) {
            return None;
        }
        match closure {
            None => {
                if let Some(known) = self.functions.get(&identity.function) {
                    return known.clone();
                }
                let plan = function_code(exe, &identity.function)
                    .and_then(|code| self.code(exe, caller, site, code));
                self.functions.insert(identity.function, plan.clone());
                plan
            }
            Some(closure) => {
                if &closure.callable != identity {
                    return None;
                }
                let key = (identity.link, identity.function);
                let found = self.closures.get(&key).and_then(|entries| {
                    entries.iter().find(|entry| {
                        entry.site == closure.site.site
                            && entry.names.as_ref() == closure.capture_names.as_slice()
                    })
                });
                if let Some(entry) = found {
                    return entry.target.clone();
                }
                // A saved closure's creation site can lie in an earlier
                // cell's main; the machine's resolver reaches it through its
                // defining Link, as every other closure read does.
                let plan = crate::machine::project::closure_code(
                    exe,
                    identity,
                    &closure.site.site,
                    &closure.capture_names,
                )
                .ok()
                .and_then(|code| self.code(exe, caller, site, code));
                self.closures.entry(key).or_default().push(ClosureEntry {
                    site: closure.site.site.clone(),
                    names: closure.capture_names.as_slice().into(),
                    target: plan.clone(),
                });
                plan
            }
        }
    }
}

/// A function value's body: its declared code, or its library's body. A
/// created code is a closure's, reached only through its creation site.
fn function_code(exe: &Executable, function: &lash_kernel_value::FunctionId) -> Option<CodeId> {
    let code = exe.declared.get(function).copied().or_else(|| {
        let lib = exe.library_lib(function)?;
        match exe.lib(lib).run {
            LibRun::Body(code)
            | LibRun::Native {
                body: Some(code), ..
            } => Some(code),
            _ => None,
        }
    })?;
    (exe.code(code).created.is_none()
        && exe.code(code).source_room.owner == doc::constant_pool::PoolOwner::Function(*function))
    .then_some(code)
}
