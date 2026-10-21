//! Resolve display keys against the package that owns them.

use std::collections::{BTreeMap, BTreeSet};

use lash_helper_labels::{LabelError, read_json};
use lash_kernel_dialect::{Export, MemberPath, Segment};
use lash_kernel_doc::FunctionName;

use crate::{PackageDisplay, Root};

pub(crate) fn read(
    data: &str,
    exports: &BTreeMap<Segment, Export>,
    roots: &BTreeSet<Root>,
) -> Result<PackageDisplay, LabelError> {
    let labels = read_json(data)?;
    let mut display = PackageDisplay::default();
    for (key, value) in labels.members {
        let refuse = || LabelError::at("/members", &key, "unknown member path");
        let member = MemberPath::try_from(key.clone()).map_err(|_| refuse())?;
        let declared = match (exports.get(&member.export), &member.member) {
            (Some(_), None) => true,
            (Some(Export::Type(ty)), Some(name)) => ty.members.contains_key(name),
            _ => false,
        };
        if !declared {
            return Err(refuse());
        }
        display.members.insert(member, value);
    }
    let backed = crate::export_members(exports);
    for (key, value) in labels.definitions {
        let name = FunctionName::new(&key)
            .map_err(|_| LabelError::at("/definitions", &key, "invalid definition name"))?;
        if !roots.iter().any(|root| root.holds(&key)) {
            return Err(LabelError::at(
                "/definitions",
                &key,
                "definition outside package roots",
            ));
        }
        if backed.iter().any(|(_, definition)| **definition == name) {
            return Err(LabelError::at(
                "/definitions",
                &key,
                "definition backs a member; label that member",
            ));
        }
        display.definitions.insert(name, value);
    }
    Ok(display)
}
