//! A document adopted onto another release of the packages it calls, within
//! its own kernel version (`K-LIB-009`).
//!
//! A package's corrected release is new code: each fixed definition is a
//! new identity, and so is every definition that calls one. Adoption
//! relinks a document by name ([`relink`]): each library function its own
//! code calls that the earlier release holds is replaced by the function the
//! later release holds under the same name, everywhere the document lists
//! and calls it, and the document then lists exactly what its code reaches
//! through the bodies it now calls.
//!
//! Adoption never moves code onto a changed operation contract: when what
//! the adopted document performs under differs from what the document did,
//! for any operation both perform, it is refused naming the operation.
//!
//! A run moves only where it stands in the document's own code, which a
//! relink does not move: the correspondence is every node of `main` and of
//! a declared function, at its own site. A parked run stands in its heap
//! pages too, and no migrator carries those, so a parked run is never
//! adopted (FIG-6233).

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_check::Performs;
use lash_kernel_doc::{
    Document, EffectName, FunctionCatalog, FunctionDefinition, FunctionId, FunctionName,
    OperationRequirement,
};

use crate::migration::{DocumentRefusal, Rewritten};
use crate::rewrite::unchanged;

/// Each function of an earlier release, with the function its successor
/// holds under the same name, or `None` when the successor holds none. A
/// function both releases hold is not listed.
pub type Relink = BTreeMap<FunctionId, Option<FunctionId>>;

/// The relink from `from`, the code of the releases a document was written
/// against, to `to`, the code of their successors, by definition name.
/// Within one package set a name is one definition: each sits under its
/// package's roots.
pub fn relink<'a>(
    from: impl IntoIterator<Item = &'a FunctionDefinition>,
    to: impl IntoIterator<Item = &'a FunctionDefinition>,
) -> Relink {
    let to: BTreeMap<&FunctionName, FunctionId> = to
        .into_iter()
        .map(|definition| (&definition.name, definition.identity()))
        .collect();
    from.into_iter()
        .filter_map(|definition| {
            let function = definition.identity();
            let successor = to.get(&definition.name).copied();
            (successor != Some(function)).then_some((function, successor))
        })
        .collect()
}

/// `base` relinked by `relink`: each library function its own code calls
/// that `relink` names replaced by its successor in every slot that holds
/// it, and its manifest listing every function the result reaches, with
/// the name its definition carries. `functions` holds every function `base`
/// reaches and every successor.
///
/// # Errors
///
/// - [`DocumentRefusal::FunctionRetired`] for a function `base` calls
///   whose successor release holds none of its name;
/// - [`DocumentRefusal::FunctionNotHeld`] for a function `functions` lacks;
/// - [`DocumentRefusal::OperationContractChanged`] when a body the result
///   reaches performs an operation under another requirement than `base`
///   states for it, or than another such body.
pub fn adopt(
    base: &Document,
    relink: &Relink,
    functions: &dyn FunctionCatalog,
) -> Result<Rewritten, DocumentRefusal> {
    let mut document = base.clone();
    document.try_each_function_mut(|_, function| {
        let Some(successor) = relink.get(function) else {
            return Ok(());
        };
        let Some(successor) = successor else {
            let definition =
                functions
                    .definition(function)
                    .ok_or(DocumentRefusal::FunctionNotHeld {
                        function: *function,
                    })?;
            return Err(DocumentRefusal::FunctionRetired {
                function: *function,
                name: definition.name.clone(),
            });
        };
        *function = *successor;
        Ok::<(), DocumentRefusal>(())
    })?;
    let required = lash_kernel_check::requirements(&document, functions);
    if let Some(function) = required
        .functions
        .keys()
        .find(|function| functions.definition(function).is_none())
    {
        return Err(DocumentRefusal::FunctionNotHeld {
            function: *function,
        });
    }
    document.manifest.functions = required.functions;
    document.manifest.effects = requirements(base, required.effects, required.performs)?;
    let correspondence = unchanged(base, &document)?;
    let carried = base
        .manifest
        .functions
        .keys()
        .map(|function| {
            let successor = relink.get(function).copied().flatten();
            (*function, successor.unwrap_or(*function))
        })
        .collect();
    Ok(Rewritten {
        document,
        correspondence,
        functions: carried,
    })
}

/// What the adopted document requires of each operation it performs: its
/// own code's operations under the requirement `base` states, and each
/// operation the bodies it reaches perform under the one requirement they
/// state (ADOPT-CONTRACT).
fn requirements(
    base: &Document,
    own: BTreeSet<EffectName>,
    performs: Performs,
) -> Result<BTreeMap<EffectName, OperationRequirement>, DocumentRefusal> {
    let mut effects = BTreeMap::new();
    for operation in own {
        if let Some(requirement) = base.manifest.effects.get(&operation) {
            effects.insert(operation, requirement.clone());
        }
    }
    for (operation, by) in performs {
        let pinned = base
            .manifest
            .effects
            .get(&operation)
            .or_else(|| by.values().next());
        let Some(pinned) = pinned.cloned() else {
            continue;
        };
        if by.values().any(|requirement| *requirement != pinned) {
            return Err(DocumentRefusal::OperationContractChanged { operation });
        }
        effects.insert(operation, pinned);
    }
    Ok(effects)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use lash_kernel_doc::{FunctionDefinition, FunctionId, parse_definition, parse_document};

    use crate::DocumentRefusal;

    /// One release of package `p`: `p.fix`, which performs `p.send` under
    /// `contract`, and `p.outer`, whose body calls it. `fixed` is a
    /// corrected release.
    fn release(fixed: bool, contract: &str) -> [FunctionDefinition; 2] {
        let body = if fixed {
            "let y = [x]\n  return y"
        } else {
            "return x"
        };
        let fix = parse_definition(&format!(
            "function p.fix(x: Any) -> Any\nkernel 1\ncharge 1\n\
             effect p.send(value?: Any) -> Any contract \"{contract}\"\n\
             body {{\n  let sent = perform p.send(x) as Any\n  {body}\n}}\n"
        ))
        .unwrap();
        let outer = parse_definition(&format!(
            "function p.outer(x: Any) -> Any\nkernel 1\ncharge 1\nuse p.fix = @{}\n\
             body {{\n  let y = invoke p.fix(x)\n  return y\n}}\n",
            fix.identity()
        ))
        .unwrap();
        [fix, outer]
    }

    /// A workflow over one release of `p`, listing what it reaches.
    fn document(release: &[FunctionDefinition; 2]) -> lash_kernel_doc::Document {
        let [fix, outer] = release;
        parse_document(&format!(
            "kernel 1\nnumbers float\neffect p.send(value?: Any) -> Any contract \"1\"\n\
             use p.fix = @{}\nuse p.outer = @{}\n\
             entry run(x: Any) -> Any\n\
             fn run(x) {{\nlet got = invoke p.outer(x)\nreturn got\n}}\n\
             main {{\nlet r = invoke p.outer(1)\nreturn r\n}}\n",
            fix.identity(),
            outer.identity()
        ))
        .unwrap()
    }

    fn catalog(releases: &[&[FunctionDefinition; 2]]) -> BTreeMap<FunctionId, FunctionDefinition> {
        releases
            .iter()
            .flat_map(|release| release.iter())
            .map(|definition| (definition.identity(), definition.clone()))
            .collect()
    }

    /// K-LIB-009: replacing identity A by B everywhere it is listed and
    /// called adopts a corrected function. Relinking a workflow written
    /// over release R1 of `p` onto R2, whose `p.fix` is corrected and whose
    /// `p.outer` is re-identified over it, gives exactly the workflow
    /// written over R2, every node of its own code at its own site.
    #[test]
    fn relinking_by_name_adopts_a_corrected_function_everywhere_it_is_listed_and_called() {
        let (r1, r2) = (release(false, "1"), release(true, "1"));
        let relink = super::relink(&r1, &r2);
        assert_eq!(
            relink,
            r1.iter()
                .zip(&r2)
                .map(|(from, to)| (from.identity(), Some(to.identity())))
                .collect(),
            "each R1 function names its R2 successor"
        );
        let base = document(&r1);
        let adopted = super::adopt(&base, &relink, &catalog(&[&r1, &r2])).unwrap();
        assert_eq!(adopted.document, document(&r2));
        let json = adopted.document.to_json().unwrap();
        for old in &r1 {
            assert!(!json.contains(&old.identity().to_string()), "{json}");
        }
        assert_eq!(adopted.correspondence.base, base.identity());
        assert_eq!(adopted.correspondence.result, document(&r2).identity());
    }

    /// A function the workflow calls that the successor release does not
    /// hold refuses the adoption, naming it.
    #[test]
    fn a_called_function_with_no_successor_refuses_naming_it() {
        let (r1, r2) = (release(false, "1"), release(true, "1"));
        let [_, outer] = &r1;
        let relink = super::relink(&r1, &r2[..1]);
        let refusal = super::adopt(&document(&r1), &relink, &catalog(&[&r1, &r2])).unwrap_err();
        assert_eq!(
            refusal,
            DocumentRefusal::FunctionRetired {
                function: outer.identity(),
                name: outer.name.clone(),
            }
        );
    }

    /// ADOPT-CONTRACT: adoption never moves a run to a changed operation
    /// contract. R2's corrected `p.fix` performs `p.send` under contract
    /// "2" where R1's performed it under "1", so the workflow is refused,
    /// naming the operation.
    #[test]
    fn adoption_never_moves_code_to_a_changed_operation_contract() {
        let (r1, r2) = (release(false, "1"), release(true, "2"));
        let refusal = super::adopt(
            &document(&r1),
            &super::relink(&r1, &r2),
            &catalog(&[&r1, &r2]),
        )
        .unwrap_err();
        assert_eq!(
            refusal,
            DocumentRefusal::OperationContractChanged {
                operation: lash_kernel_doc::EffectName::new("p.send").unwrap(),
            }
        );
    }
}
