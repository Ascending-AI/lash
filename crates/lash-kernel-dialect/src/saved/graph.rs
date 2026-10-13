//! Saved functions as checkpoint graph nodes (FIG-5887 §3, §4).
//!
//! A session keeps each function it holds as one immutable
//! [`Bundle`]: the function and every saved function it uses, transitively,
//! each once, in a member table sorted by the name each is declared under.
//! Functions that use one another share one member table, and a member
//! links to another by its index there, so no bundle names another bundle
//! and no content hash is cyclic. A member's document is a node of its own,
//! shared by every bundle that holds it; its captures and token are frozen
//! data in the member's row, and the release its helpers come from is named
//! by an exact manifest reference.
//!
//! This module writes and reads the bundle's logical body. Nodes are staged
//! by the caller's `stage` function, so this crate depends on no store.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::graph_tokens::{
    ChildRef, CodecVariant, TokenCodec, TokenError, TokenReader, TokenWriter, check_ascending,
    encode_body,
};
use lash_kernel_doc::{Datum, Document, EffectName, Name, Signature, ValueKind};

use super::{CaptureRefusal, Constant, NotSaved, SavedFunction, Token, Written};

/// The registry number of a Document node (`checkpoint_graph::NodeKind::DOCUMENT`).
pub const DOCUMENT_NODE_KIND: u32 = 10;

/// The registry number of a FunctionBundle node
/// (`checkpoint_graph::NodeKind::FUNCTION_BUNDLE`).
pub const FUNCTION_BUNDLE_NODE_KIND: u32 = 30;

/// A saved function's bundle: it and every saved function it uses,
/// transitively, by the name each is declared under.
#[derive(Clone, Debug, PartialEq)]
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
    /// binding as [`super::closure_of`] reads them. A function a member uses
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

    /// The effects the members perform, each with the signature its member
    /// expects: sorted by effect, then by member, each pair once.
    #[must_use]
    pub fn required_effects(&self) -> Vec<(EffectName, Signature)> {
        let mut required: Vec<(EffectName, Signature)> = Vec::new();
        let mut by_effect: BTreeMap<&EffectName, Vec<&Signature>> = BTreeMap::new();
        for function in self.members.values() {
            for (effect, signature) in &function.document.manifest.effects {
                let signatures = by_effect.entry(effect).or_default();
                if !signatures.contains(&signature) {
                    signatures.push(signature);
                }
            }
        }
        for (effect, signatures) in by_effect {
            required.extend(
                signatures
                    .into_iter()
                    .map(|signature| (effect.clone(), signature.clone())),
            );
        }
        required
    }

    /// Stages the bundle: each member's document as a Document node, then
    /// the bundle itself, naming `release`, the exact manifest of the helper
    /// release its members were written against. Answers the bundle's
    /// reference.
    ///
    /// # Errors
    ///
    /// The first error `stage` answers.
    pub fn stage<E>(
        &self,
        release: ChildRef,
        mut stage: impl FnMut(u32, &[u8]) -> Result<ChildRef, E>,
    ) -> Result<ChildRef, E> {
        let mut members = Vec::with_capacity(self.members.len());
        for (name, function) in &self.members {
            let document = stage(DOCUMENT_NODE_KIND, &encode_body(&function.document))?;
            members.push(MemberBody {
                name: name.clone(),
                document,
                captures: function.captures.clone(),
                helper_owners: function.helper_owners.clone(),
                written: function.written.clone(),
                token: function.token.clone(),
            });
        }
        let body = BundleBody {
            members,
            effects: self.required_effects(),
            release,
        };
        stage(FUNCTION_BUNDLE_NODE_KIND, &encode_body(&body))
    }

    /// The bundle a decoded body holds, its documents read by `document`,
    /// and the release it names. A member must be declared by its own
    /// document, and the required effects must be exactly the members'.
    ///
    /// # Errors
    ///
    /// The first error `document` answers, or a [`BundleRefusal`].
    pub fn assemble<E: From<BundleRefusal>>(
        body: BundleBody,
        mut document: impl FnMut(ChildRef) -> Result<Document, E>,
    ) -> Result<(Self, ChildRef), E> {
        let mut members = BTreeMap::new();
        for member in body.members {
            let function = SavedFunction {
                name: member.name.clone(),
                document: document(member.document)?,
                captures: member.captures,
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
        Ok((bundle, body.release))
    }
}

/// A FunctionBundle node's logical body.
#[derive(Clone, Debug, PartialEq)]
pub struct BundleBody {
    /// Sorted by name, each once.
    pub members: Vec<MemberBody>,
    /// [`Bundle::required_effects`].
    pub effects: Vec<(EffectName, Signature)>,
    /// The exact ReleaseManifest the members' helpers come from.
    pub release: ChildRef,
}

/// One member's row: a [`SavedFunction`] with its document by reference.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberBody {
    pub name: Name,
    pub document: ChildRef,
    pub captures: BTreeMap<Name, Datum>,
    pub helper_owners: BTreeSet<Name>,
    pub written: Option<Written>,
    pub token: Option<Token>,
}

impl TokenCodec for BundleBody {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.members.encode(out);
        out.seq(self.effects.len() as u64);
        for (effect, signature) in &self.effects {
            out.seq(2);
            effect.encode(out);
            signature.encode(out);
        }
        self.release.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
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
            effects.push((EffectName::decode(input)?, Signature::decode(input)?));
        }
        Ok(Self {
            members,
            effects,
            release: ChildRef::decode(input)?,
        })
    }
}

impl TokenCodec for MemberBody {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(6);
        self.name.encode(out);
        self.document.encode(out);
        self.captures.encode(out);
        self.helper_owners.encode(out);
        self.written.encode(out);
        self.token.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(6)?;
        Ok(Self {
            name: Name::decode(input)?,
            document: ChildRef::decode(input)?,
            captures: BTreeMap::decode(input)?,
            helper_owners: BTreeSet::decode(input)?,
            written: Option::decode(input)?,
            token: Option::decode(input)?,
        })
    }
}

impl TokenCodec for Written {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(5);
        out.text(&self.dialect);
        self.signature.encode(out);
        self.source.encode(out);
        self.start.encode(out);
        self.metadata.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(5)?;
        Ok(Self {
            dialect: String::decode(input)?,
            signature: Option::decode(input)?,
            source: Option::decode(input)?,
            start: Option::decode(input)?,
            metadata: Option::decode(input)?,
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

impl TokenCodec for Constant {
    fn encode(&self, out: &mut TokenWriter) {
        self.0.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Self::try_from(Datum::decode(input)?).map_err(|why| TokenError::Invalid(why.to_owned()))
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

#[doc(hidden)]
impl CodecVariant for CaptureRefusal {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Task => 0,
            Self::Function => 1,
            Self::Cyclic => 2,
            Self::Conflict => 3,
            Self::NoConstant { .. } => 4,
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

impl TokenCodec for CaptureRefusal {
    fn encode(&self, out: &mut TokenWriter) {
        let variant = self.codec_variant();
        match self {
            Self::Task | Self::Function | Self::Cyclic | Self::Conflict => out.sum(variant, 0),
            Self::NoConstant { kind } => {
                out.sum(variant, 1);
                out.sum(value_kind_variant(*kind), 0);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        let arity = |expected| TokenReader::expect_fields(fields, expected);
        Ok(match variant {
            0 => arity(0).map(|()| Self::Task)?,
            1 => arity(0).map(|()| Self::Function)?,
            2 => arity(0).map(|()| Self::Cyclic)?,
            3 => arity(0).map(|()| Self::Conflict)?,
            4 => {
                arity(1)?;
                let (kind, fields) = input.sum()?;
                TokenReader::expect_fields(fields, 0)?;
                Self::NoConstant {
                    kind: value_kind(kind)?,
                }
            }
            variant => return Err(TokenError::UnsupportedVariant { variant }),
        })
    }
}

/// [`ValueKind`]'s numbers: the Value discriminants of FIG-5887 §2.
const VALUE_KINDS: [ValueKind; 19] = [
    ValueKind::Null,
    ValueKind::Absent,
    ValueKind::Bool,
    ValueKind::Int,
    ValueKind::Float,
    ValueKind::Text,
    ValueKind::Bytes,
    ValueKind::Timestamp,
    ValueKind::Tuple,
    ValueKind::List,
    ValueKind::Map,
    ValueKind::Set,
    ValueKind::Record,
    ValueKind::Closure,
    ValueKind::Error,
    ValueKind::Task,
    ValueKind::Function,
    ValueKind::Handle,
    ValueKind::Ref,
];

fn value_kind_variant(kind: ValueKind) -> u32 {
    match kind {
        ValueKind::Null => 0,
        ValueKind::Absent => 1,
        ValueKind::Bool => 2,
        ValueKind::Int => 3,
        ValueKind::Float => 4,
        ValueKind::Text => 5,
        ValueKind::Bytes => 6,
        ValueKind::Timestamp => 7,
        ValueKind::Tuple => 8,
        ValueKind::List => 9,
        ValueKind::Map => 10,
        ValueKind::Set => 11,
        ValueKind::Record => 12,
        ValueKind::Closure => 13,
        ValueKind::Error => 14,
        ValueKind::Task => 15,
        ValueKind::Function => 16,
        ValueKind::Handle => 17,
        ValueKind::Ref => 18,
    }
}

fn value_kind(variant: u32) -> Result<ValueKind, TokenError> {
    usize::try_from(variant)
        .ok()
        .and_then(|at| VALUE_KINDS.get(at).copied())
        .ok_or(TokenError::UnsupportedVariant { variant })
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;
