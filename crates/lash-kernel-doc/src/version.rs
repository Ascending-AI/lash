//! Kernel versions as values (`K-VER-001`, `K-VER-003`).
//!
//! A build interprets the newest kernel version and, for the one release
//! after a breaking version ships, the version before it. Everything two
//! versions differ in is an arm of a `match` on [`KernelVersion`]: how a
//! form is spelled in the stored encoding (this crate), what an operation
//! costs (`lash-kernel-vm`), and the migration between them
//! (`lash-kernel-migrate`). The older version's interpreter is the sum of
//! its arms. It is deleted in one place, its variant below: with the
//! variant gone, every arm that served it stops compiling and is removed.

use crate::document::KERNEL_VERSION;

/// A kernel version this build interprets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KernelVersion {
    /// Kernel version 1: `docs/kernel/semantics.md` as written.
    One,
    /// The successor only a `synthetic-next` build knows. It differs from
    /// version 1 in two pinned rules: the stored encoding spells the
    /// `print` statement `emit`, and a test of a loop's continuation costs
    /// 2. It exists to prove the migration machinery before a real second
    /// version does; nothing is written in it but what a migration writes.
    #[cfg(feature = "synthetic-next")]
    SyntheticNext,
}

impl KernelVersion {
    /// Every version this build interprets, oldest first.
    pub const ALL: &'static [Self] = &[
        Self::One,
        #[cfg(feature = "synthetic-next")]
        Self::SyntheticNext,
    ];

    /// The newest version this build interprets: the one a parked run is
    /// carried to.
    #[cfg(not(feature = "synthetic-next"))]
    pub const NEWEST: Self = Self::One;
    /// The newest version this build interprets: the one a parked run is
    /// carried to.
    #[cfg(feature = "synthetic-next")]
    pub const NEWEST: Self = Self::SyntheticNext;

    /// The number a document, a definition and a parked run state.
    pub const fn number(self) -> u32 {
        match self {
            Self::One => KERNEL_VERSION,
            #[cfg(feature = "synthetic-next")]
            Self::SyntheticNext => KERNEL_VERSION + 1,
        }
    }

    /// The version numbered `number`, when this build interprets it.
    pub fn of(number: u32) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|version| version.number() == number)
    }

    /// The version this one replaced, when this build still interprets it.
    pub const fn previous(self) -> Option<Self> {
        match self {
            Self::One => None,
            #[cfg(feature = "synthetic-next")]
            Self::SyntheticNext => Some(Self::One),
        }
    }
}

impl std::fmt::Display for KernelVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.number())
    }
}

/// The stored spelling of the forms `synthetic-next` renames.
///
/// The form tree is one Rust type for every version; serde derives version
/// 1's spelling. A document or definition written for the synthetic
/// successor is stored with its `print` statements spelled `emit`, so its
/// bytes and its identity are the successor's own and a version 1 reader
/// cannot decode them.
#[cfg(feature = "synthetic-next")]
pub(crate) mod synthetic {
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use serde_json::Value;

    use super::KernelVersion;
    use crate::canonical::EncodeError;
    use crate::document::DecodeError;

    /// `print` is a tag of the statement enum and of nothing else in a
    /// block: no other form, field or type of the grammar is spelled so.
    const ONE: &str = "print";
    const NEXT: &str = "emit";

    /// The path from a document or a definition to each of its blocks'
    /// roots. Names a host chose are keys only above these.
    fn blocks(tree: &mut Value, visit: &mut dyn FnMut(&mut Value)) {
        let Some(members) = tree.as_object_mut() else {
            return;
        };
        if let Some(main) = members.get_mut("main") {
            visit(main);
        }
        if let Some(Value::Object(functions)) = members.get_mut("functions") {
            for function in functions.values_mut() {
                if let Some(body) = function.get_mut("body") {
                    visit(body);
                }
            }
        }
        if let Some(Value::Object(implementation)) = members.get_mut("implementation") {
            for body in implementation.values_mut() {
                if let Some(block) = body.get_mut("block") {
                    visit(block);
                }
            }
        }
    }

    /// Renames the key `from` to `to` throughout `tree`. Returns whether
    /// `to` was already a key somewhere, which the spelling does not allow.
    fn rename(tree: &mut Value, from: &str, to: &str) -> bool {
        match tree {
            Value::Array(items) => items
                .iter_mut()
                .fold(false, |clash, item| rename(item, from, to) | clash),
            Value::Object(members) => {
                let mut clash = members.contains_key(to);
                if let Some(member) = members.remove(from) {
                    members.insert(to.to_owned(), member);
                }
                for member in members.values_mut() {
                    clash |= rename(member, from, to);
                }
                clash
            }
            _ => false,
        }
    }

    fn states(tree: &Value) -> Option<u32> {
        let kernel = tree
            .get("manifest")
            .map_or_else(|| tree.get("kernel"), |manifest| manifest.get("kernel"))?;
        u32::try_from(kernel.as_u64()?).ok()
    }

    /// Respells `tree`, the derived encoding of a document or definition,
    /// when it states the synthetic successor. Returns whether it does.
    pub(crate) fn spell(tree: &mut Value) -> bool {
        if states(tree) != Some(KernelVersion::SyntheticNext.number()) {
            return false;
        }
        blocks(tree, &mut |block| {
            rename(block, ONE, NEXT);
        });
        true
    }

    /// The stored tree of a document or definition written for the
    /// synthetic successor, or `None` for one written for another version.
    pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Option<Value>, EncodeError> {
        let mut tree = serde_json::to_value(value).map_err(|error| EncodeError {
            message: error.to_string(),
        })?;
        Ok(spell(&mut tree).then_some(tree))
    }

    /// Decodes `text` when it states the synthetic successor, or answers
    /// `None` for the caller to decode as written.
    pub(crate) fn decode<T: DeserializeOwned>(text: &str) -> Result<Option<T>, DecodeError> {
        let invalid = |message: String| DecodeError::Invalid {
            message,
            line: 0,
            column: 0,
        };
        let mut decoder = serde_json::Deserializer::from_str(text);
        decoder.disable_recursion_limit();
        let Ok(mut tree) = <Value as serde::Deserialize>::deserialize(&mut decoder) else {
            // Not JSON: the caller's decoder says where.
            return Ok(None);
        };
        if states(&tree) != Some(KernelVersion::SyntheticNext.number()) {
            return Ok(None);
        }
        let mut clash = false;
        blocks(&mut tree, &mut |block| clash |= rename(block, NEXT, ONE));
        if clash {
            return Err(invalid(format!(
                "unknown variant `{ONE}`: kernel version {} spells it `{NEXT}`",
                KernelVersion::SyntheticNext
            )));
        }
        serde_json::from_value(tree)
            .map(Some)
            .map_err(|error| invalid(error.to_string()))
    }
}
