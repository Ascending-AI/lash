//! The executable identity of one compiled entry point (FIG-3571).
//!
//! It names what a VM runs: the module artifact, the entry point compiled out
//! of it, and every build contract that decides the instruction stream and the
//! node ids that stream reports. Two builds that would run different code for
//! the same entry mint different identities, so anything a VM parks — a
//! continuation, a process segment — carries this value and is refused by a
//! build that mints another.
//!
//! The identity is a pure function of its inputs and this build's constants:
//! a host computes it from a module ref and an entry without loading or
//! compiling the artifact, which is what lets a durable engine fence a retired
//! generation before any other step of a run.

use serde::{Deserialize, Serialize};

use crate::{BYTECODE_FORMAT_VERSION, LASHLANG_SEMANTIC_HASH_VERSION, ModuleRef};

use super::entry_points::Entry;

/// The domain the node ids a compiled program reports are minted under
/// ([`crate::workflow_node_id`]). It is part of the identity so a node-id
/// change retires what the previous ids were recorded against.
const WORKFLOW_NODE_DOMAIN: &str = "lash-workflow-node/v3";

/// The executable identity of one compiled entry point.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecutableIdentity(String);

impl ExecutableIdentity {
    /// The identity this build mints for `entry` of the module `module_ref`.
    #[expect(
        clippy::expect_used,
        reason = "the preimage is a tuple of strings and integer constants serialized straight to in-memory bytes"
    )]
    pub fn of(module_ref: &ModuleRef, entry: Entry<'_>) -> Self {
        let entry = match entry {
            Entry::Main => ("main", String::new(), 0),
            Entry::Process(process_ref) => (
                "process",
                process_ref.component.to_string(),
                process_ref.pos,
            ),
        };
        let preimage = serde_json::to_vec(&(
            LASHLANG_SEMANTIC_HASH_VERSION,
            BYTECODE_FORMAT_VERSION,
            super::INSTRUCTION_ACCOUNTING_VERSION,
            WORKFLOW_NODE_DOMAIN,
            module_ref.as_str(),
            entry,
        ))
        .expect("the executable identity preimage should serialize");
        Self(format!(
            "blake3:{}",
            lash_sansio::core_support::blake3_domain_hash_hex(
                "lash-lashlang-executable/v1",
                preimage,
            )
        ))
    }

    /// The identity of a program compiled from a bare AST, with no module
    /// around it: only this crate's own tests compile one.
    #[cfg(test)]
    pub(crate) fn unlinked() -> Self {
        Self("unlinked".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ExecutableIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentHash, ProcessRef};

    fn module(tag: &str) -> ModuleRef {
        ModuleRef::new(&ContentHash::new(tag))
    }

    #[test]
    fn the_node_domain_is_the_one_node_ids_are_minted_under() {
        let expected = {
            let mut hasher =
                lash_sansio::core_support::Blake3DomainHasher::new(WORKFLOW_NODE_DOMAIN);
            hasher.update("owner".as_bytes());
            hasher.update([0]);
            hasher.update(7u32.to_be_bytes());
            format!("node:{}", &hasher.finalize_hex()[..24])
        };
        assert_eq!(crate::workflow_node_id("owner", &[7]).as_str(), expected);
    }

    #[test]
    fn every_input_separates_identities() {
        let first = ProcessRef::new(ContentHash::new("c"), 0);
        let second = ProcessRef::new(ContentHash::new("c"), 1);
        let identities = [
            ExecutableIdentity::of(&module("a"), Entry::Main),
            ExecutableIdentity::of(&module("b"), Entry::Main),
            ExecutableIdentity::of(&module("a"), Entry::Process(&first)),
            ExecutableIdentity::of(&module("a"), Entry::Process(&second)),
        ];
        let distinct = identities
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        assert_eq!(distinct, identities.len());
        assert_eq!(
            ExecutableIdentity::of(&module("a"), Entry::Process(&first)),
            ExecutableIdentity::of(&module("a"), Entry::Process(&first)),
            "the identity is a pure function of its inputs"
        );
    }
}
