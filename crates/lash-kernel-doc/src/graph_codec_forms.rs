//! The binary codecs of the kernel's types, forms, documents and
//! library-function definitions (FIG-5887 §3): node kinds 10 and 14.
//! The rules are [`super`]'s.

use std::collections::{BTreeMap, BTreeSet};

use super::{unsupported, variant};
use crate::{
    ActExpr, ActKind, Action, AssignOrder, Atom, Bytes, Callee, Catch, Closure, Document, Expr,
    Float, Formula, FunctionBody, FunctionDefinition, FunctionId, Guard, Implementation, Integer,
    JoinMode, Literal, MapEntry, Member, NumberPolicy, Operand, Place, Protocol, RecordEntry, Rhs,
    Stmt, TryStmt,
};
use lash_kernel_value::names::Name;

use lash_kernel_value::types::Signature;
use lash_kernel_value::types::Type;
use {
    lash_kernel_value::tokens::CodecVariant, lash_kernel_value::tokens::TokenCodec,
    lash_kernel_value::tokens::TokenError, lash_kernel_value::tokens::TokenReader,
    lash_kernel_value::tokens::TokenWriter,
};

use crate::{
    CallablePresentation, Export, FunctionExport, FunctionRepresentation, FunctionTarget,
    FunctionValue, StartConvention, TypeDef, TypeMember,
};

use {lash_kernel_value::claims::NodeRequirements, lash_kernel_value::names::*};

// Forms.

impl TokenCodec for Literal {
    fn encode(&self, out: &mut TokenWriter) {
        let number = self.codec_variant();
        match self {
            Self::Null | Self::Absent => out.sum(number, 0),
            Self::Bool(value) => {
                out.sum(number, 1);
                out.bool(*value);
            }
            Self::Int(value) => {
                out.sum(number, 1);
                value.encode(out);
            }
            Self::Float(value) => {
                out.sum(number, 1);
                value.encode(out);
            }
            Self::Text(value) => {
                out.sum(number, 1);
                out.text(value);
            }
            Self::Bytes(value) => {
                out.sum(number, 1);
                value.encode(out);
            }
            Self::Function(name) => {
                out.sum(number, 1);
                name.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let number = variant(input, |variant| match variant {
            0 | 1 => Some(0),
            2..=7 => Some(1),
            _ => None,
        })?;
        Ok(match number {
            0 => Self::Null,
            1 => Self::Absent,
            2 => Self::Bool(input.bool()?),
            3 => Self::Int(Integer::decode(input)?),
            4 => Self::Float(Float::decode(input)?),
            5 => Self::Text(String::decode(input)?),
            6 => Self::Bytes(Bytes::decode(input)?),
            _ => Self::Function(FunctionId::decode(input)?),
        })
    }
}

impl TokenCodec for Atom {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(self.codec_variant(), 1);
        match self {
            Self::Variable(name) => name.encode(out),
            Self::Literal(literal) => literal.encode(out),
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| (variant <= 1).then_some(1))? {
                0 => Self::Variable(Name::decode(input)?),
                _ => Self::Literal(Literal::decode(input)?),
            },
        )
    }
}

impl TokenCodec for Callee {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(self.codec_variant(), 1);
        match self {
            Self::Value(name) => name.encode(out),
            Self::Import(slot) => slot.encode(out),
            Self::Local(id) => id.encode(out),
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| (variant <= 2).then_some(1))? {
                0 => Self::Local(FunctionId::decode(input)?),
                1 => Self::Value(Name::decode(input)?),
                _ => Self::Import(ImportSlot::decode(input)?),
            },
        )
    }
}

impl TokenCodec for Action {
    fn encode(&self, out: &mut TokenWriter) {
        let number = self.codec_variant();
        match self {
            Self::Call { callee, args } | Self::Spawn { callee, args } => {
                out.sum(number, 2);
                callee.encode(out);
                args.encode(out);
            }
            Self::Perform {
                slot, args, result, ..
            } => {
                out.sum(number, 3);
                slot.encode(out);
                args.encode(out);
                result.encode(out);
            }
            Self::Join { task: atom } | Self::Cancel { task: atom } => {
                out.sum(number, 1);
                atom.encode(out);
            }
            Self::Await { policy, value } => {
                out.sum(number, 2);
                policy.encode(out);
                value.encode(out);
            }
            Self::JoinMany { mode, tasks } => {
                out.sum(number, 2);
                mode.encode(out);
                tasks.encode(out);
            }
            Self::Yield => out.sum(number, 0),
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let number = variant(input, |variant| match variant {
            0 | 4 | 6 | 10 => Some(2),
            1 | 8 => Some(3),
            3 | 7 => Some(1),
            5 => Some(0),
            _ => None,
        })?;
        Ok(match number {
            0 => Self::Call {
                callee: Callee::decode(input)?,
                args: Vec::decode(input)?,
            },
            1 | 8 => Self::Perform {
                slot: ImportSlot::decode(input)?,
                args: Vec::decode(input)?,
                result: Type::decode(input)?,
                then_yield: number == 8,
            },
            10 => Self::Await {
                policy: FunctionTarget::decode(input)?,
                value: Atom::decode(input)?,
            },
            3 => Self::Join {
                task: Atom::decode(input)?,
            },
            4 => Self::JoinMany {
                mode: JoinMode::decode(input)?,
                tasks: Atom::decode(input)?,
            },
            5 => Self::Yield,
            6 => Self::Spawn {
                callee: Callee::decode(input)?,
                args: Vec::decode(input)?,
            },
            _ => Self::Cancel {
                task: Atom::decode(input)?,
            },
        })
    }
}

impl TokenCodec for Member {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(self.codec_variant(), 2);
        match self {
            Self::Field { target, field } => {
                target.encode(out);
                out.text(field);
            }
            Self::Index { target, index } => {
                target.encode(out);
                index.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| (variant <= 1).then_some(2))? {
                0 => Self::Field {
                    target: Expr::decode(input)?,
                    field: String::decode(input)?,
                },
                _ => Self::Index {
                    target: Expr::decode(input)?,
                    index: Expr::decode(input)?,
                },
            },
        )
    }
}

impl TokenCodec for Place {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(self.codec_variant(), 1);
        match self {
            Self::Variable(name) => name.encode(out),
            Self::Member(member) => member.encode(out),
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| (variant <= 1).then_some(1))? {
                0 => Self::Variable(Name::decode(input)?),
                _ => Self::Member(Member::decode(input)?),
            },
        )
    }
}

impl TokenCodec for MapEntry {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.key.encode(out);
        self.value.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            key: Expr::decode(input)?,
            value: Expr::decode(input)?,
        })
    }
}

impl TokenCodec for RecordEntry {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.text(&self.field);
        self.value.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            field: String::decode(input)?,
            value: Expr::decode(input)?,
        })
    }
}

impl TokenCodec for ActKind {
    fn encode(&self, out: &mut TokenWriter) {
        let number = self.codec_variant();
        match self {
            Self::Call(callee) | Self::Spawn(callee) => {
                out.sum(number, 1);
                callee.encode(out);
            }
            Self::Perform { slot, result, .. } => {
                out.sum(number, 2);
                slot.encode(out);
                result.encode(out);
            }
            Self::Join | Self::Yield | Self::Cancel => out.sum(number, 0),
            Self::JoinMany(mode) => {
                out.sum(number, 1);
                mode.encode(out);
            }
            Self::Await(policy) => {
                out.sum(number, 1);
                policy.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let number = variant(input, |variant| match variant {
            0 | 3 | 5 | 6 => Some(1),
            1 | 7 => Some(2),
            4 | 9 | 10 => Some(0),
            _ => None,
        })?;
        Ok(match number {
            0 => Self::Call(Callee::decode(input)?),
            1 | 7 => Self::Perform {
                slot: ImportSlot::decode(input)?,
                result: Type::decode(input)?,
                then_yield: number == 7,
            },
            3 => Self::Spawn(Callee::decode(input)?),
            4 => Self::Join,
            5 => Self::JoinMany(JoinMode::decode(input)?),
            6 => Self::Await(FunctionTarget::decode(input)?),
            9 => Self::Yield,
            _ => Self::Cancel,
        })
    }
}

impl TokenCodec for ActExpr {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.kind.encode(out);
        self.args.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            kind: ActKind::decode(input)?,
            args: Vec::decode(input)?,
        })
    }
}

impl TokenCodec for Closure {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.function.encode(out);
        self.presentation.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            function: FunctionId::decode(input)?,
            presentation: Option::decode(input)?,
        })
    }
}

impl TokenCodec for CallablePresentation {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.signature.encode(out);
        self.representation.encode(out);
        self.convention.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            signature: Signature::decode(input)?,
            representation: FunctionRepresentation::decode(input)?,
            convention: StartConvention::decode(input)?,
        })
    }
}

/// Bare (0) or a token (1) of its tag and callable slot.
impl TokenCodec for FunctionRepresentation {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Bare => out.sum(0, 0),
            Self::Token { tag, callable } => {
                out.sum(1, 2);
                out.text(tag);
                out.unsigned(u64::from(*callable));
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |v| match v {
                0 => Some(0),
                1 => Some(2),
                _ => None,
            })? {
                0 => Self::Bare,
                _ => Self::Token {
                    tag: String::decode(input)?,
                    callable: input.unsigned_u32()?,
                },
            },
        )
    }
}

/// Direct (0), TypeScript's argument list (1), source parameters (2), or
/// direct with a root cleanup (3). TypeScript adapters retain the helpers
/// extraction needs.
impl TokenCodec for StartConvention {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Direct => out.sum(0, 0),
            Self::PyArgsList { positional } => {
                out.sum(4, 1);
                positional.encode(out);
            }
            Self::TsDirect { await_fn } => {
                out.sum(2, 1);
                await_fn.encode(out);
            }
            Self::TsArgsList { await_fn, same_fn } => {
                out.sum(1, 2);
                await_fn.encode(out);
                same_fn.encode(out);
            }
            Self::PyDirect { cleanup } => {
                out.sum(3, 1);
                cleanup.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |v| match v {
                0 => Some(0),
                1 => Some(2),
                2 => Some(1),
                3 => Some(1),
                4 => Some(1),
                _ => None,
            })? {
                0 => Self::Direct,
                4 => Self::PyArgsList {
                    positional: u32::decode(input)?,
                },
                2 => Self::TsDirect {
                    await_fn: FunctionTarget::decode(input)?,
                },
                3 => Self::PyDirect {
                    cleanup: FunctionTarget::decode(input)?,
                },
                _ => Self::TsArgsList {
                    await_fn: FunctionTarget::decode(input)?,
                    same_fn: FunctionTarget::decode(input)?,
                },
            },
        )
    }
}

/// A function value: its document and its entry's name.
impl TokenCodec for FunctionValue {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(11);
        self.callable.encode(out);
        self.captures.encode(out);
        self.signature.encode(out);
        self.convention.encode(out);
        self.captions.encode(out);
        self.representation.encode(out);
        self.creation.encode(out);
        self.token.encode(out);
        self.signature_origin.encode(out);
        self.implicit_text_conversions.encode(out);
        self.written_signature.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(11)?;
        Ok(Self {
            callable: lash_kernel_value::CallableId::decode(input)?,
            captures: BTreeMap::decode(input)?,
            signature: Signature::decode(input)?,
            convention: StartConvention::decode(input)?,
            captions: crate::Captions::decode(input)?,
            representation: FunctionRepresentation::decode(input)?,
            creation: Option::decode(input)?,
            token: Option::decode(input)?,
            signature_origin: Option::decode(input)?,
            implicit_text_conversions: Vec::decode(input)?,
            written_signature: Option::decode(input)?,
        })
    }
}

impl TokenCodec for Expr {
    fn encode(&self, out: &mut TokenWriter) {
        let number = self.codec_variant();
        match self {
            Self::Literal(literal) => {
                out.sum(number, 1);
                literal.encode(out);
            }
            Self::Variable(name) => {
                out.sum(number, 1);
                name.encode(out);
            }
            Self::Tuple(items) | Self::List(items) | Self::Set(items) => {
                out.sum(number, 1);
                items.encode(out);
            }
            Self::Map(entries) => {
                out.sum(number, 1);
                entries.encode(out);
            }
            Self::Record(entries) => {
                out.sum(number, 1);
                entries.encode(out);
            }
            Self::Member(member) => {
                out.sum(number, 1);
                member.encode(out);
            }
            Self::Closure(closure) => {
                out.sum(number, 1);
                closure.encode(out);
            }
            Self::Call { function, args } => {
                out.sum(number, 2);
                function.encode(out);
                args.encode(out);
            }
            Self::If { cond, then, r#else } => {
                out.sum(number, 3);
                cond.encode(out);
                then.encode(out);
                r#else.encode(out);
            }
            Self::Let { name, value, body } => {
                out.sum(number, 3);
                name.encode(out);
                value.encode(out);
                body.encode(out);
            }
            Self::Act(act) => {
                out.sum(number, 1);
                act.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            let number = variant(input, |variant| match variant {
                0..=8 | 12 | 15 => Some(1),
                9 => Some(2),
                13 | 14 => Some(3),

                _ => None,
            })?;
            Ok(match number {
                0 => Self::Literal(Literal::decode(input)?),
                1 => Self::Variable(Name::decode(input)?),
                2 => Self::Tuple(Vec::decode(input)?),
                3 => Self::List(Vec::decode(input)?),
                4 => Self::Map(Vec::decode(input)?),
                5 => Self::Set(Vec::decode(input)?),
                6 => Self::Record(Vec::decode(input)?),
                7 => Self::Member(Box::decode(input)?),
                8 => Self::Closure(Box::decode(input)?),
                9 => Self::Call {
                    function: FunctionId::decode(input)?,
                    args: Vec::decode(input)?,
                },
                13 => Self::If {
                    cond: Box::decode(input)?,
                    then: Box::decode(input)?,
                    r#else: Box::decode(input)?,
                },
                14 => Self::Let {
                    name: Name::decode(input)?,
                    value: Box::decode(input)?,
                    body: Box::decode(input)?,
                },
                _ => Self::Act(Box::decode(input)?),
            })
        })
    }
}

impl TokenCodec for Rhs {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(self.codec_variant(), 1);
        match self {
            Self::Expr(expr) => expr.encode(out),
            Self::Action(action) => action.encode(out),
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| (variant <= 1).then_some(1))? {
                0 => Self::Expr(Expr::decode(input)?),
                _ => Self::Action(Action::decode(input)?),
            },
        )
    }
}

impl TokenCodec for Catch {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.binding.encode(out);
        self.body.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            binding: Name::decode(input)?,
            body: Vec::decode(input)?,
        })
    }
}

impl TokenCodec for Protocol {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        self.open.encode(out);
        self.next.encode(out);
        self.item.encode(out);
        self.check.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        Ok(Self {
            open: FunctionTarget::decode(input)?,
            next: Option::decode(input)?,
            item: Option::decode(input)?,
            check: Option::decode(input)?,
        })
    }
}

impl TokenCodec for TryStmt {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.body.encode(out);
        self.catch.encode(out);
        self.finally.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            body: Vec::decode(input)?,
            catch: Option::decode(input)?,
            finally: Option::decode(input)?,
        })
    }
}

impl TokenCodec for Stmt {
    fn encode(&self, out: &mut TokenWriter) {
        let number = self.codec_variant();
        match self {
            Self::Let { name, value } => {
                out.sum(number, 2);
                name.encode(out);
                value.encode(out);
            }
            Self::Assign { place, value, .. } => {
                out.sum(number, 2);
                place.encode(out);
                value.encode(out);
            }
            Self::Remove { member } => {
                out.sum(number, 1);
                member.encode(out);
            }
            Self::Do { value } => {
                out.sum(number, 1);
                value.encode(out);
            }
            Self::If {
                condition,
                then_block,
                else_block,
            } => {
                out.sum(number, 3);
                condition.encode(out);
                then_block.encode(out);
                else_block.encode(out);
            }
            Self::For {
                metered,
                binding,
                iterable,
                via,
                body,
                else_block,
            } => {
                let extended = number != 5;
                out.sum(
                    number,
                    if number == 19 {
                        6
                    } else if extended {
                        5
                    } else {
                        3
                    },
                );
                if number == 19 {
                    metered.encode(out);
                }
                binding.encode(out);
                iterable.encode(out);
                if extended {
                    via.encode(out);
                }
                body.encode(out);
                if extended {
                    else_block.encode(out);
                }
            }
            Self::While {
                metered,
                condition,
                body,
                else_block,
            } => {
                let extended = number != 6;
                out.sum(
                    number,
                    if number == 20 {
                        4
                    } else if extended {
                        3
                    } else {
                        2
                    },
                );
                if number == 20 {
                    metered.encode(out);
                }
                condition.encode(out);
                body.encode(out);
                if extended {
                    else_block.encode(out);
                }
            }
            Self::Break | Self::Continue => out.sum(number, 0),
            Self::Return { value }
            | Self::Throw { value }
            | Self::Print { value }
            | Self::Finish { value }
            | Self::Fail { value } => {
                out.sum(number, 1);
                value.encode(out);
            }
            Self::ReturnVia { value, policy } => {
                out.sum(number, 2);
                value.encode(out);
                policy.encode(out);
            }
            Self::Try(scope) => {
                out.sum(number, 1);
                scope.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            let number = variant(input, |variant| match variant {
                0 | 1 | 6 | 17 | 18 => Some(2),
                2 | 3 | 9..=14 => Some(1),
                4 | 5 | 16 => Some(3),
                15 => Some(5),
                19 => Some(6),
                20 => Some(4),
                7 | 8 => Some(0),
                _ => None,
            })?;
            Ok(match number {
                0 => Self::Let {
                    name: Name::decode(input)?,
                    value: Rhs::decode(input)?,
                },
                1 => Self::Assign {
                    place: Place::decode(input)?,
                    value: Rhs::decode(input)?,
                    order: AssignOrder::ValueFirst,
                },
                18 => Self::Assign {
                    place: Place::decode(input)?,
                    value: Rhs::decode(input)?,
                    order: AssignOrder::PlaceFirst,
                },
                2 => Self::Remove {
                    member: Member::decode(input)?,
                },
                3 => Self::Do {
                    value: Expr::decode(input)?,
                },
                4 => Self::If {
                    condition: Expr::decode(input)?,
                    then_block: Vec::decode(input)?,
                    else_block: Vec::decode(input)?,
                },
                5 => Self::For {
                    metered: 0,
                    binding: Name::decode(input)?,
                    iterable: Expr::decode(input)?,
                    via: None,
                    body: Vec::decode(input)?,
                    else_block: Vec::new(),
                },
                6 => Self::While {
                    metered: 0,
                    condition: Expr::decode(input)?,
                    body: Vec::decode(input)?,
                    else_block: Vec::new(),
                },
                15 | 19 => {
                    let stmt = Self::For {
                        metered: if number == 19 { u64::decode(input)? } else { 0 },
                        binding: Name::decode(input)?,
                        iterable: Expr::decode(input)?,
                        via: Option::decode(input)?,
                        body: Vec::decode(input)?,
                        else_block: Vec::decode(input)?,
                    };
                    // A loop with neither has the one encoding, number 5.
                    if stmt.codec_variant() != number {
                        return Err(unsupported(number));
                    }
                    stmt
                }
                16 | 20 => {
                    let stmt = Self::While {
                        metered: if number == 20 { u64::decode(input)? } else { 0 },
                        condition: Expr::decode(input)?,
                        body: Vec::decode(input)?,
                        else_block: Vec::decode(input)?,
                    };
                    if stmt.codec_variant() != number {
                        return Err(unsupported(number));
                    }
                    stmt
                }
                7 => Self::Break,
                8 => Self::Continue,
                9 => Self::Return {
                    value: Expr::decode(input)?,
                },
                17 => Self::ReturnVia {
                    value: Expr::decode(input)?,
                    policy: FunctionTarget::decode(input)?,
                },
                10 => Self::Try(TryStmt::decode(input)?),
                11 => Self::Throw {
                    value: Expr::decode(input)?,
                },
                12 => Self::Print {
                    value: Expr::decode(input)?,
                },
                13 => Self::Finish {
                    value: Expr::decode(input)?,
                },
                14 => Self::Fail {
                    value: Expr::decode(input)?,
                },
                _ => return Err(unsupported(number)),
            })
        })
    }
}

fn decode_grammar(input: &mut TokenReader<'_>) -> Result<(), TokenError> {
    let found = input.unsigned_u32()?;
    if found != crate::DOCUMENT_GRAMMAR_VERSION {
        return Err(super::invalid(format!(
            "document token grammar {found} is unsupported; this build accepts {}",
            crate::DOCUMENT_GRAMMAR_VERSION,
        )));
    }
    Ok(())
}

// Documents.

impl TokenCodec for Document {
    fn encode(&self, out: &mut TokenWriter) {
        crate::document_work::note(|work| work.token_encodes += 1);
        out.seq(9);
        out.unsigned(u64::from(crate::DOCUMENT_GRAMMAR_VERSION));
        self.library.0.encode(out);
        self.imports.encode(out);
        self.exports.encode(out);
        self.main.encode(out);
        self.functions.encode(out);
        self.requires.encode(out);
        self.numbers.encode(out);
        self.private_bindings.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        crate::document_work::note(|work| work.token_decodes += 1);
        input.expect_struct(9)?;
        decode_grammar(input)?;
        Ok(Self {
            source_evidence: BTreeMap::new(),
            library: crate::LibraryDependencies(BTreeSet::decode(input)?),
            imports: BTreeMap::decode(input)?,
            exports: BTreeMap::decode(input)?,
            main: Option::decode(input)?,
            functions: BTreeMap::decode(input)?,
            requires: NodeRequirements::decode(input)?,
            numbers: NumberPolicy::decode(input)?,
            private_bindings: BTreeSet::decode(input)?,
        })
    }
}

/// A JSON value, as a host keeps it in a kernel body: a sum of null (0), bool
/// (1), number (2), string (3), array (4) and object (5). A number is a sum
/// of non-negative integer (0), negative integer (1) and finite float (2),
/// each in exactly one spelling. An object's keys ascend by raw bytes.
impl TokenCodec for Operand {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Param(name) => {
                out.sum(self.codec_variant(), 1);
                name.encode(out);
            }
            Self::Result => out.sum(self.codec_variant(), 0),
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| match variant {
                0 => Some(1),
                1 => Some(0),
                _ => None,
            })? {
                0 => Self::Param(Name::decode(input)?),
                _ => Self::Result,
            },
        )
    }
}

impl TokenCodec for Formula {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(self.codec_variant(), 1);
        match self {
            Self::Constant(amount) => out.unsigned(*amount),
            Self::CeilLog2(term) => term.encode(out),
            Self::Size(operand)
            | Self::DeepSize(operand)
            | Self::NestedSize(operand)
            | Self::Magnitude(operand) => operand.encode(out),
            Self::Sum(terms) | Self::Product(terms) | Self::Max(terms) | Self::Min(terms) => {
                terms.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            Ok(
                match variant(input, |variant| (variant <= 9).then_some(1))? {
                    0 => Self::Constant(input.unsigned()?),
                    1 => Self::Size(Operand::decode(input)?),
                    2 => Self::DeepSize(Operand::decode(input)?),
                    3 => Self::NestedSize(Operand::decode(input)?),
                    4 => Self::Magnitude(Operand::decode(input)?),
                    5 => Self::Sum(Vec::decode(input)?),
                    6 => Self::Product(Vec::decode(input)?),
                    7 => Self::Max(Vec::decode(input)?),
                    8 => Self::Min(Vec::decode(input)?),
                    _ => Self::CeilLog2(Box::new(Self::decode(input)?)),
                },
            )
        })
    }
}

impl TokenCodec for Guard {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.text(&self.unit);
        self.limit.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            unit: String::decode(input)?,
            limit: Formula::decode(input)?,
        })
    }
}

impl TokenCodec for FunctionBody {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.functions.encode(out);
        self.imports.encode(out);
        self.block.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            functions: BTreeMap::decode(input)?,
            imports: BTreeMap::decode(input)?,
            block: Vec::decode(input)?,
        })
    }
}

impl TokenCodec for crate::Metering {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Statements => out.sum(0, 0),
            Self::Formula(formula) => {
                out.sum(1, 1);
                formula.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |v| match v {
                0 => Some(0),
                1 => Some(1),
                _ => None,
            })? {
                0 => Self::Statements,
                _ => Self::Formula(Formula::decode(input)?),
            },
        )
    }
}
impl TokenCodec for Implementation {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Native { charge } => {
                out.sum(0, 1);
                charge.encode(out);
            }
            Self::Guest { body, metering } => {
                out.sum(1, 2);
                metering.encode(out);
                body.encode(out);
            }
            Self::Both { body, charge } => {
                out.sum(2, 2);
                charge.encode(out);
                body.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |v| match v {
                0 => Some(1),
                1 | 2 => Some(2),
                _ => None,
            })? {
                0 => Self::Native {
                    charge: Formula::decode(input)?,
                },
                1 => {
                    let metering = crate::Metering::decode(input)?;
                    Self::Guest {
                        body: FunctionBody::decode(input)?,
                        metering,
                    }
                }
                _ => {
                    let charge = Formula::decode(input)?;
                    Self::Both {
                        body: FunctionBody::decode(input)?,
                        charge,
                    }
                }
            },
        )
    }
}
impl TokenCodec for crate::ProjectionRole {
    fn encode(&self, out: &mut TokenWriter) {
        out.unsigned(match self {
            Self::Authored => 0,
            Self::ArgumentCopy => 1,
            Self::ResultDecode => 2,
            Self::DurationMilliseconds => 3,
            Self::TimerSeconds => 4,
            Self::ImplicitReturn => 5,
        });
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match input.unsigned_u32()? {
            0 => Self::Authored,
            1 => Self::ArgumentCopy,
            2 => Self::ResultDecode,
            3 => Self::DurationMilliseconds,
            4 => Self::TimerSeconds,
            5 => Self::ImplicitReturn,
            other => return Err(unsupported(other)),
        })
    }
}

/// Read only the header and implementation metering; skip the executable body.
pub(crate) fn decode_header(bytes: &[u8]) -> Result<crate::FunctionHeader, TokenError> {
    let mut input = TokenReader::new(bytes)
        .with_data_limits(lash_kernel_value::data_limits::KernelDataLimits::standard());
    input.expect_struct(8)?;
    let kernel = input.unsigned_u32()?;
    let name = FunctionName::decode(&mut input)?;
    let signature = Signature::decode(&mut input)?;
    let errors = BTreeSet::decode(&mut input)?;
    let guard = Option::decode(&mut input)?;
    let projection = crate::ProjectionRole::decode(&mut input)?;
    let kind = variant(&mut input, |v| match v {
        0 => Some(1),
        1 | 2 => Some(2),
        _ => None,
    })?;
    let metering = if kind == 1 {
        crate::Metering::decode(&mut input)?
    } else {
        crate::Metering::Formula(Formula::decode(&mut input)?)
    };
    Ok(crate::FunctionHeader {
        projection,
        kernel,
        name,
        signature,
        errors,
        guard,
        metering,
        implementation: match kind {
            0 => crate::ImplementationKind::Native,
            1 => crate::ImplementationKind::Guest,
            _ => crate::ImplementationKind::Both,
        },
    })
}

/// Node kind 14's typed body.
impl TokenCodec for FunctionDefinition {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(8);
        out.unsigned(u64::from(self.kernel));
        self.name.encode(out);
        self.signature.encode(out);
        self.errors.encode(out);
        self.guard.encode(out);
        self.projection.encode(out);
        self.implementation.encode(out);
        out.unsigned(u64::from(self.native_version));
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(8)?;
        Ok(Self {
            kernel: input.unsigned_u32()?,
            name: FunctionName::decode(input)?,
            signature: Signature::decode(input)?,
            errors: BTreeSet::decode(input)?,
            guard: Option::decode(input)?,
            projection: crate::ProjectionRole::decode(input)?,
            implementation: Implementation::decode(input)?,
            native_version: input.unsigned_u32()?,
        })
    }
}

macro_rules! record {
    ($ty:ty { $($field:ident),+ }) => {
        impl TokenCodec for $ty {
            fn encode(&self,out:&mut TokenWriter) {out.seq([$(stringify!($field)),+].len() as u64);$(self.$field.encode(out);)+}
            fn decode(input:&mut TokenReader<'_>)->Result<Self,TokenError> {
                input.expect_struct([$(stringify!($field)),+].len() as u64)?;
                Ok(Self {$($field:TokenCodec::decode(input)?),+})
            }
        }
    };
}
record!(FunctionExport {
    function,
    signature,
    convention,
    asynchronous,
    rest
});
record!(TypeDef {
    brand,
    representation,
    checker,
    members
});
impl TokenCodec for FunctionTarget {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Local(id) => {
                out.sum(0, 1);
                id.encode(out);
            }
            Self::Import(slot) => {
                out.sum(1, 1);
                slot.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match variant(input, |v| (v <= 1).then_some(1))? {
            0 => Self::Local(FunctionId::decode(input)?),
            _ => Self::Import(ImportSlot::decode(input)?),
        })
    }
}
impl TokenCodec for Export {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Function(value) => {
                out.sum(0, 1);
                value.encode(out);
            }
            Self::Type(value) => {
                out.sum(1, 1);
                value.encode(out);
            }
            Self::Error { kind } => {
                out.sum(2, 1);
                kind.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match variant(input, |v| (v <= 2).then_some(1))? {
            0 => Self::Function(FunctionExport::decode(input)?),
            1 => Self::Type(TypeDef::decode(input)?),
            _ => Self::Error {
                kind: Brand::decode(input)?,
            },
        })
    }
}
impl TokenCodec for TypeMember {
    fn encode(&self, out: &mut TokenWriter) {
        let (kind, value) = match self {
            Self::Static(v) => (0, v),
            Self::Method(v) => (1, v),
            Self::Property(v) => (2, v),
        };
        out.sum(kind, 1);
        value.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let kind = variant(input, |v| (v <= 2).then_some(1))?;
        let value = FunctionExport::decode(input)?;
        Ok(match kind {
            0 => Self::Static(value),
            1 => Self::Method(value),
            _ => Self::Property(value),
        })
    }
}

impl TokenCodec for crate::WrittenSignature {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.language.encode(out);
        self.signature.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            language: crate::SourceLanguage::decode(input)?,
            signature: String::decode(input)?,
        })
    }
}
