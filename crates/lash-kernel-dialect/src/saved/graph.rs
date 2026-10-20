//! Saved function bundles reference immutable capture pages (S8).
//!
//! A session keeps each function it holds as one immutable
//! [`Bundle`]: the function and every saved function it uses, transitively,
//! each once, in a member table sorted by the name each is declared under.
//! Functions that use one another share one member table, and a member
//! refers to another by name, so no bundle names another bundle and no
//! content hash is cyclic. A member's document is a node of its own, shared
//! by every bundle that holds it; its captures and token data are frozen
//! page references in the member's row, and the library functions it reaches
//! are named by its execution closure node.
//!
//! This module writes and reads the bundle's logical body. Nodes are staged
//! by the caller's `stage` function, so this crate depends on no store.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::graph_tokens::{
    ChildRef, CodecVariant, TokenCodec, TokenError, TokenReader, TokenWriter, check_ascending,
    encode_body,
};
use lash_kernel_doc::heap::PageRef;
use lash_kernel_doc::{Document, EffectName, Name, OperationRequirement};
use lash_kernel_state::paged::FrozenRef;

use super::{CaptureRefusal, NotSaved, SavedFunction, Token};
use crate::Written;

/// The registry number of a Document node (`checkpoint_graph::NodeKind::DOCUMENT`).
pub const DOCUMENT_NODE_KIND: u32 = 10;

/// The registry number of a FunctionBundle node
/// (`checkpoint_graph::NodeKind::FUNCTION_BUNDLE`).
pub const FUNCTION_BUNDLE_NODE_KIND: u32 = 30;

/// A saved function's bundle: it and every saved function it uses,
/// transitively, by the name each is declared under.
#[derive(Clone, Debug)]
pub struct Bundle {
    members: BTreeMap<Name, SavedFunction>,
}

/// Why a decoded bundle is not one a session holds.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BundleRefusal {
    /// A member's document does not declare the member.
    #[error("bundle member `{member}`'s document does not declare it")]
    Undeclared { member: Name },
    /// The required effects are not the members' own.
    #[error("the bundle's required effects are not its members'")]
    RequiredEffects,
}

impl Bundle {
    /// The bundle of the function bound to `root` in `functions`, keyed by
    /// binding under their saved names. A function a member uses
    /// that `functions` does not hold is left out: a cell that installs the
    /// member is refused for it as ever ([`super::Unusable::Missing`]).
    /// `None` when `root` holds no function.
    #[must_use]
    pub fn of(functions: &BTreeMap<Name, SavedFunction>, root: &Name) -> Option<Self> {
        let first = functions.get(root)?;
        let mut members = BTreeMap::new();
        let mut pending = vec![first];
        while let Some(function) = pending.pop() {
            if members.contains_key(&function.name) {
                continue;
            }
            pending.extend(
                function
                    .needs()
                    .iter()
                    .filter_map(|needed| functions.get(needed)),
            );
            members.insert(function.name.clone(), function.clone());
        }
        Some(Self { members })
    }

    /// Every member, by the name it is declared under.
    #[must_use]
    pub fn members(&self) -> &BTreeMap<Name, SavedFunction> {
        &self.members
    }

    /// The index of the member declared under `name`.
    #[must_use]
    pub fn index(&self, name: &Name) -> Option<u64> {
        self.members
            .keys()
            .position(|member| member == name)
            .map(|at| at as u64)
    }

    /// The member at `index`.
    #[must_use]
    pub fn member(&self, index: u64) -> Option<&SavedFunction> {
        usize::try_from(index)
            .ok()
            .and_then(|at| self.members.values().nth(at))
    }

    /// The effects the members perform, each with what its member requires
    /// of it: sorted by effect, then by member, each pair once.
    #[must_use]
    pub fn required_effects(&self) -> Vec<(EffectName, OperationRequirement)> {
        let mut required: Vec<(EffectName, OperationRequirement)> = Vec::new();
        let mut by_effect: BTreeMap<&EffectName, Vec<&OperationRequirement>> = BTreeMap::new();
        for function in self.members.values() {
            for (effect, requirement) in &function.document.manifest.effects {
                let requirements = by_effect.entry(effect).or_default();
                if !requirements.contains(&requirement) {
                    requirements.push(requirement);
                }
            }
        }
        for (effect, requirements) in by_effect {
            required.extend(
                requirements
                    .into_iter()
                    .map(|requirement| (effect.clone(), requirement.clone())),
            );
        }
        required
    }

    /// Stages the bundle: each member's document as a Document node, then
    /// the bundle itself. Each member names its execution closure and
    /// frozen capture pages. Answers the bundle's
    /// reference.
    ///
    /// # Errors
    ///
    /// The first error `stage` answers.
    pub fn stage<E>(
        &self,
        mut stage: impl FnMut(u32, &[u8]) -> Result<ChildRef, E>,
    ) -> Result<ChildRef, E> {
        let mut members = Vec::with_capacity(self.members.len());
        for (name, function) in &self.members {
            let document = stage(DOCUMENT_NODE_KIND, &encode_body(&function.document))?;
            members.push(MemberBody {
                name: name.clone(),
                document,
                captures: function.captures.clone(),
                closure: function.closure,
                references: function.references.clone(),
                helper_owners: function.helper_owners.clone(),
                written: function.written.clone(),
                token: function.token.clone(),
            });
        }
        let body = BundleBody {
            members,
            effects: self.required_effects(),
        };
        stage(FUNCTION_BUNDLE_NODE_KIND, &encode_body(&body))
    }

    /// The bundle a decoded body holds, its documents read by `document`,
    /// and the execution closure each member names. A member must be declared by its own
    /// document, and the required effects must be exactly the members'.
    ///
    /// # Errors
    ///
    /// The first error `document` answers, or a [`BundleRefusal`].
    pub fn assemble<E: From<BundleRefusal>>(
        body: BundleBody,
        mut document: impl FnMut(ChildRef) -> Result<Document, E>,
    ) -> Result<Self, E> {
        let mut members = BTreeMap::new();
        for member in body.members {
            let function = SavedFunction {
                name: member.name.clone(),
                document: document(member.document)?,
                captures: member.captures,
                closure: member.closure,
                references: member.references,
                helper_owners: member.helper_owners,
                written: member.written,
                token: member.token,
            };
            if function.definition().is_none() {
                return Err(BundleRefusal::Undeclared {
                    member: member.name,
                }
                .into());
            }
            members.insert(member.name, function);
        }
        let bundle = Self { members };
        if bundle.required_effects() != body.effects {
            return Err(BundleRefusal::RequiredEffects.into());
        }
        Ok(bundle)
    }
}

/// A FunctionBundle node's logical body.
#[derive(Clone, Debug)]
pub struct BundleBody {
    /// Sorted by name, each once.
    pub members: Vec<MemberBody>,
    /// [`Bundle::required_effects`].
    pub effects: Vec<(EffectName, OperationRequirement)>,
}

/// One member's row: a [`SavedFunction`] with its document by reference.
#[derive(Clone, Debug)]
pub struct MemberBody {
    pub name: Name,
    pub document: ChildRef,
    pub captures: BTreeMap<Name, FrozenRef>,
    pub closure: PageRef,
    pub references: BTreeSet<Name>,
    pub helper_owners: BTreeSet<Name>,
    pub written: Option<Written>,
    pub token: Option<Token>,
}

impl TokenCodec for BundleBody {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.members.encode(out);
        out.seq(self.effects.len() as u64);
        for (effect, requirement) in &self.effects {
            out.seq(2);
            effect.encode(out);
            requirement.encode(out);
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let members = Vec::<MemberBody>::decode(input)?;
        check_ascending(
            &members
                .iter()
                .map(|member| &member.name)
                .collect::<Vec<_>>(),
        )?;
        let count = input.seq()?;
        let mut effects = Vec::new();
        for _ in 0..count {
            input.expect_struct(2)?;
            effects.push((
                EffectName::decode(input)?,
                OperationRequirement::decode(input)?,
            ));
        }
        Ok(Self { members, effects })
    }
}

impl TokenCodec for MemberBody {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(8);
        self.name.encode(out);
        self.document.encode(out);
        self.captures.encode(out);
        self.closure.encode(out);
        self.references.encode(out);
        self.helper_owners.encode(out);
        self.written.encode(out);
        self.token.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(8)?;
        Ok(Self {
            name: Name::decode(input)?,
            document: ChildRef::decode(input)?,
            captures: BTreeMap::decode(input)?,
            closure: PageRef::decode(input)?,
            references: BTreeSet::decode(input)?,
            helper_owners: BTreeSet::decode(input)?,
            written: Option::decode(input)?,
            token: Option::decode(input)?,
        })
    }
}

impl TokenCodec for Token {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        out.text(&self.tag);
        self.before.encode(out);
        self.after.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            tag: String::decode(input)?,
            before: Vec::decode(input)?,
            after: Vec::decode(input)?,
        })
    }
}

/// Explicit codec variant numbers, the zero-based declaration order at
/// revision `9503e220ed` (FIG-5887 §2).
#[doc(hidden)]
impl CodecVariant for NotSaved {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Task => 0,
            Self::FunctionInData => 1,
            Self::MadeByLibrary => 2,
            Self::Capture { .. } => 3,
            Self::Needs { .. } => 4,
            Self::Token => 5,
            Self::PerformsControl { .. } => 6,
            Self::Dialect => 7,
            Self::NotMigrated { .. } => 8,
            Self::Unreadable { .. } => 9,
        }
    }
}

impl TokenCodec for NotSaved {
    fn encode(&self, out: &mut TokenWriter) {
        let variant = self.codec_variant();
        match self {
            Self::Task
            | Self::FunctionInData
            | Self::MadeByLibrary
            | Self::Token
            | Self::Dialect => out.sum(variant, 0),
            Self::Capture { name, why } => {
                out.sum(variant, 2);
                name.encode(out);
                why.encode(out);
            }
            Self::Needs { function } => {
                out.sum(variant, 1);
                function.encode(out);
            }
            Self::PerformsControl { effect } => {
                out.sum(variant, 1);
                effect.encode(out);
            }
            Self::NotMigrated { from, problem } => {
                out.sum(variant, 2);
                from.encode(out);
                out.text(problem);
            }
            Self::Unreadable { problem } => {
                out.sum(variant, 1);
                out.text(problem);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        let arity = |expected| TokenReader::expect_fields(fields, expected);
        Ok(match variant {
            0 => arity(0).map(|()| Self::Task)?,
            1 => arity(0).map(|()| Self::FunctionInData)?,
            2 => arity(0).map(|()| Self::MadeByLibrary)?,
            3 => {
                arity(2)?;
                Self::Capture {
                    name: Name::decode(input)?,
                    why: CaptureRefusal::decode(input)?,
                }
            }
            4 => {
                arity(1)?;
                Self::Needs {
                    function: Name::decode(input)?,
                }
            }
            5 => arity(0).map(|()| Self::Token)?,
            6 => {
                arity(1)?;
                Self::PerformsControl {
                    effect: EffectName::decode(input)?,
                }
            }
            7 => arity(0).map(|()| Self::Dialect)?,
            8 => {
                arity(2)?;
                Self::NotMigrated {
                    from: u32::decode(input)?,
                    problem: String::decode(input)?,
                }
            }
            9 => {
                arity(1)?;
                Self::Unreadable {
                    problem: String::decode(input)?,
                }
            }
            variant => return Err(TokenError::UnsupportedVariant { variant }),
        })
    }
}
