//! The binary codecs of the kernel's types, forms, documents and
//! library-function definitions (FIG-5887 §3): node kinds 10 and 14.
//! The rules are [`super`]'s.

use std::collections::{BTreeMap, BTreeSet};

use super::{invalid, unsupported, variant};
use crate::graph_tokens::{
    CanonicalFault, CodecVariant, TokenCodec, TokenError, TokenReader, TokenWriter,
};
use crate::{
    ActExpr, ActKind, Action, AssignOrder, Atom, Block, Bytes, Callee, Catch, Closure, Document,
    Expr, Float, Formula, Function, FunctionBody, FunctionDefinition, FunctionId, Guard,
    Implementation, Integer, JoinMode, Literal, Manifest, MapEntry, MapType, Member, Name,
    NumberPolicy, Operand, Param, Place, ProjectionRead, Protocol, QualifiedName, RecordEntry,
    RecordType, RecordTypeField, Rhs, Signature, Stmt, TryStmt, Type,
};
use crate::{FunctionRepresentation, FunctionValue, StartAdapter, StartConvention};

impl TokenCodec for crate::Entry {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.signature.encode(out);
        self.convention.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            signature: Signature::decode(input)?,
            convention: StartConvention::decode(input)?,
        })
    }
}

// Types and signatures.

impl TokenCodec for Type {
    fn encode(&self, out: &mut TokenWriter) {
        let number = self.codec_variant();
        match self {
            Self::Any
            | Self::Null
            | Self::Absent
            | Self::Bool
            | Self::Int
            | Self::Float
            | Self::Number
            | Self::Text
            | Self::Bytes
            | Self::Timestamp
            | Self::Error => out.sum(number, 0),
            Self::Tuple(types) | Self::Union(types) => {
                out.sum(number, 1);
                types.encode(out);
            }
            Self::List(inner) | Self::Set(inner) | Self::Task(inner) => {
                out.sum(number, 1);
                inner.encode(out);
            }
            Self::Map(map) => {
                out.sum(number, 1);
                map.encode(out);
            }
            Self::Record(record) => {
                out.sum(number, 1);
                record.encode(out);
            }
            Self::Enum(texts) => {
                out.sum(number, 1);
                texts.encode(out);
            }
            Self::Function(signature) => {
                out.sum(number, 1);
                signature.encode(out);
            }
            Self::Handle(kind) => {
                out.sum(number, 1);
                out.text(kind);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            let number = variant(input, |variant| match variant {
                0..=9 | 18 => Some(0),
                10..=17 | 19 | 20 => Some(1),
                _ => None,
            })?;
            Ok(match number {
                0 => Self::Any,
                1 => Self::Null,
                2 => Self::Absent,
                3 => Self::Bool,
                4 => Self::Int,
                5 => Self::Float,
                6 => Self::Number,
                7 => Self::Text,
                8 => Self::Bytes,
                9 => Self::Timestamp,
                10 => Self::Tuple(Vec::decode(input)?),
                11 => Self::List(Box::decode(input)?),
                12 => Self::Map(Box::decode(input)?),
                13 => Self::Set(Box::decode(input)?),
                14 => Self::Record(RecordType::decode(input)?),
                15 => Self::Enum(Vec::decode(input)?),
                16 => Self::Function(Box::decode(input)?),
                17 => Self::Task(Box::decode(input)?),
                18 => Self::Error,
                19 => Self::Handle(String::decode(input)?),
                _ => Self::Union(Vec::decode(input)?),
            })
        })
    }
}

impl TokenCodec for MapType {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.key.encode(out);
        self.value.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            key: Type::decode(input)?,
            value: Type::decode(input)?,
        })
    }
}

impl TokenCodec for RecordType {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.fields.encode(out);
        self.rest.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            fields: Vec::decode(input)?,
            rest: Option::decode(input)?,
        })
    }
}

impl TokenCodec for RecordTypeField {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        out.text(&self.name);
        self.ty.encode(out);
        out.bool(self.optional);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            name: String::decode(input)?,
            ty: Type::decode(input)?,
            optional: input.bool()?,
        })
    }
}

impl TokenCodec for Param {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.name.encode(out);
        self.ty.encode(out);
        out.bool(self.optional);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            name: Name::decode(input)?,
            ty: Type::decode(input)?,
            optional: input.bool()?,
        })
    }
}

impl TokenCodec for Signature {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.params.encode(out);
        self.result.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            params: Vec::decode(input)?,
            result: Type::decode(input)?,
        })
    }
}

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
            _ => Self::Function(Name::decode(input)?),
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
            Self::Declared(name) | Self::Value(name) => name.encode(out),
            Self::Library(id) => id.encode(out),
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| (variant <= 2).then_some(1))? {
                0 => Self::Declared(Name::decode(input)?),
                1 => Self::Value(Name::decode(input)?),
                _ => Self::Library(FunctionId::decode(input)?),
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
                effect,
                args,
                result,
                ..
            } => {
                out.sum(number, 3);
                effect.encode(out);
                args.encode(out);
                result.encode(out);
            }
            Self::Sleep { duration: atom, .. }
            | Self::Join { task: atom }
            | Self::Cancel { task: atom } => {
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
            2 | 3 | 7 | 9 => Some(1),
            5 => Some(0),
            _ => None,
        })?;
        Ok(match number {
            0 => Self::Call {
                callee: Callee::decode(input)?,
                args: Vec::decode(input)?,
            },
            1 | 8 => Self::Perform {
                effect: QualifiedName::decode(input)?,
                args: Vec::decode(input)?,
                result: Type::decode(input)?,
                then_yield: number == 8,
            },
            2 | 9 => Self::Sleep {
                duration: Atom::decode(input)?,
                then_yield: number == 9,
            },
            10 => Self::Await {
                policy: FunctionId::decode(input)?,
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

impl TokenCodec for ProjectionRead {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.handle.encode(out);
        self.request.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            handle: Expr::decode(input)?,
            request: Expr::decode(input)?,
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
            Self::Perform { effect, result, .. } => {
                out.sum(number, 2);
                effect.encode(out);
                result.encode(out);
            }
            Self::Sleep { .. } | Self::Join | Self::Yield | Self::Cancel => out.sum(number, 0),
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
            2 | 4 | 8 | 9 | 10 => Some(0),
            _ => None,
        })?;
        Ok(match number {
            0 => Self::Call(Callee::decode(input)?),
            1 | 7 => Self::Perform {
                effect: QualifiedName::decode(input)?,
                result: Type::decode(input)?,
                then_yield: number == 7,
            },
            2 | 8 => Self::Sleep {
                then_yield: number == 8,
            },
            3 => Self::Spawn(Callee::decode(input)?),
            4 => Self::Join,
            5 => Self::JoinMany(JoinMode::decode(input)?),
            6 => Self::Await(FunctionId::decode(input)?),
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
        encode_code(out, &self.params, &self.body, self.start.as_ref());
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (params, body, start) = decode_code(input)?;
        Ok(Self {
            params,
            body,
            start,
        })
    }
}

/// Code's parameters and body, then its start adapter only when it has
/// one: code that states none keeps the identity it had (`K-CLO-004`), and
/// each value has one encoding.
fn encode_code(
    out: &mut TokenWriter,
    params: &impl TokenCodec,
    body: &impl TokenCodec,
    start: Option<&StartAdapter>,
) {
    out.seq(if start.is_some() { 3 } else { 2 });
    params.encode(out);
    body.encode(out);
    if let Some(start) = start {
        start.encode(out);
    }
}

fn decode_code(
    input: &mut TokenReader<'_>,
) -> Result<(Vec<Name>, Block, Option<StartAdapter>), TokenError> {
    let fields = input.seq()?;
    if !matches!(fields, 2 | 3) {
        return Err(CanonicalFault::FieldCount {
            expected: 2,
            found: fields,
        }
        .into());
    }
    let params = Vec::decode(input)?;
    let body = Vec::decode(input)?;
    let start = if fields == 3 {
        Some(StartAdapter::decode(input)?)
    } else {
        None
    };
    Ok((params, body, start))
}

impl TokenCodec for StartAdapter {
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

/// Direct (0), TypeScript's argument list (1), or source parameters (2).
/// TypeScript adapters retain the helpers extraction needs.
impl TokenCodec for StartConvention {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Direct => out.sum(0, 0),
            Self::TsDirect { await_fn } => {
                out.sum(2, 1);
                await_fn.encode(out);
            }
            Self::TsArgsList { await_fn, same_fn } => {
                out.sum(1, 2);
                await_fn.encode(out);
                same_fn.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |v| match v {
                0 => Some(0),
                1 => Some(2),
                2 => Some(1),
                _ => None,
            })? {
                0 => Self::Direct,
                2 => Self::TsDirect {
                    await_fn: FunctionId::decode(input)?,
                },
                _ => Self::TsArgsList {
                    await_fn: FunctionId::decode(input)?,
                    same_fn: FunctionId::decode(input)?,
                },
            },
        )
    }
}

/// A function value: its document and its entry's name.
impl TokenCodec for FunctionValue {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.document.encode(out);
        self.entry.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            document: Document::decode(input)?,
            entry: Name::decode(input)?,
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
            Self::Clock | Self::Random => out.sum(number, 0),
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
            Self::Read(read) => {
                out.sum(number, 1);
                read.encode(out);
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
                10 | 11 => Some(0),
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
                10 => Self::Clock,
                11 => Self::Random,
                12 => Self::Read(Box::decode(input)?),
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
            open: FunctionId::decode(input)?,
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
                binding,
                iterable,
                via,
                body,
                else_block,
            } => {
                let extended = number == 15;
                out.sum(number, if extended { 5 } else { 3 });
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
                condition,
                body,
                else_block,
            } => {
                let extended = number == 16;
                out.sum(number, if extended { 3 } else { 2 });
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
                    binding: Name::decode(input)?,
                    iterable: Expr::decode(input)?,
                    via: None,
                    body: Vec::decode(input)?,
                    else_block: Vec::new(),
                },
                6 => Self::While {
                    condition: Expr::decode(input)?,
                    body: Vec::decode(input)?,
                    else_block: Vec::new(),
                },
                15 => {
                    let stmt = Self::For {
                        binding: Name::decode(input)?,
                        iterable: Expr::decode(input)?,
                        via: Option::decode(input)?,
                        body: Vec::decode(input)?,
                        else_block: Vec::decode(input)?,
                    };
                    // A loop with neither has the one encoding, number 5.
                    if stmt.codec_variant() != 15 {
                        return Err(unsupported(15));
                    }
                    stmt
                }
                16 => {
                    let stmt = Self::While {
                        condition: Expr::decode(input)?,
                        body: Vec::decode(input)?,
                        else_block: Vec::decode(input)?,
                    };
                    if stmt.codec_variant() != 16 {
                        return Err(unsupported(16));
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
                    policy: FunctionId::decode(input)?,
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

impl TokenCodec for Function {
    fn encode(&self, out: &mut TokenWriter) {
        encode_code(out, &self.params, &self.body, self.start.as_ref());
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (params, body, start) = decode_code(input)?;
        Ok(Self {
            params,
            body,
            start,
        })
    }
}

// Documents.

impl TokenCodec for Manifest {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        out.unsigned(u64::from(self.kernel));
        self.effects.encode(out);
        self.functions.encode(out);
        self.numbers.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        Ok(Self {
            kernel: input.unsigned_u32()?,
            effects: BTreeMap::decode(input)?,
            functions: BTreeMap::decode(input)?,
            numbers: NumberPolicy::decode(input)?,
        })
    }
}

/// Node kind 10's typed body, and the document's canonical form: its
/// identity, [`Document::identity`], is this body's logical digest.
impl TokenCodec for Document {
    fn encode(&self, out: &mut TokenWriter) {
        crate::document_work::note(|work| work.token_encodes += 1);
        out.seq(5);
        self.manifest.encode(out);
        self.functions.encode(out);
        self.entries.encode(out);
        self.private_bindings.encode(out);
        self.main.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        crate::document_work::note(|work| work.token_decodes += 1);
        input.expect_struct(5)?;
        Ok(Self {
            manifest: Manifest::decode(input)?,
            functions: BTreeMap::decode(input)?,
            entries: BTreeMap::decode(input)?,
            private_bindings: BTreeSet::decode(input)?,
            main: Vec::decode(input)?,
        })
    }
}

/// A JSON value, as a host keeps it in a kernel body: a sum of null (0), bool
/// (1), number (2), string (3), array (4) and object (5). A number is a sum
/// of non-negative integer (0), negative integer (1) and finite float (2),
/// each in exactly one spelling. An object's keys ascend by raw bytes.
impl TokenCodec for serde_json::Value {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Null => out.sum(0, 0),
            Self::Bool(value) => {
                out.sum(1, 1);
                out.bool(*value);
            }
            Self::Number(number) => {
                out.sum(2, 1);
                if let Some(unsigned) = number.as_u64() {
                    out.sum(0, 1);
                    out.unsigned(unsigned);
                } else if let Some(signed) = number.as_i64() {
                    out.sum(1, 1);
                    out.signed(signed);
                } else {
                    out.sum(2, 1);
                    out.float(number.as_f64().unwrap_or(f64::NAN));
                }
            }
            Self::String(text) => {
                out.sum(3, 1);
                out.text(text);
            }
            Self::Array(items) => {
                out.sum(4, 1);
                items.encode(out);
            }
            Self::Object(entries) => {
                out.sum(5, 1);
                let mut sorted: Vec<(&String, &serde_json::Value)> = entries.iter().collect();
                sorted.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
                out.map(sorted.len() as u64);
                for (key, value) in sorted {
                    out.text(key);
                    value.encode(out);
                }
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            let number = variant(input, |variant| match variant {
                0 => Some(0),
                1..=5 => Some(1),
                _ => None,
            })?;
            Ok(match number {
                0 => Self::Null,
                1 => Self::Bool(input.bool()?),
                2 => Self::Number(
                    match variant(input, |variant| (variant <= 2).then_some(1))? {
                        0 => serde_json::Number::from(input.unsigned()?),
                        1 => {
                            let signed = input.signed()?;
                            if signed >= 0 {
                                return Err(invalid("a negative JSON integer is not negative"));
                            }
                            serde_json::Number::from(signed)
                        }
                        _ => {
                            let float = input.float()?;
                            let number = serde_json::Number::from_f64(float)
                                .ok_or_else(|| invalid("a JSON number is not finite"))?;
                            if number.as_u64().is_some() || number.as_i64().is_some() {
                                return Err(invalid("a JSON float spells an integer"));
                            }
                            number
                        }
                    },
                ),
                3 => Self::String(String::decode(input)?),
                4 => Self::Array(Vec::decode(input)?),
                _ => {
                    let count = input.map()?;
                    let mut entries = serde_json::Map::new();
                    let mut last: Option<String> = None;
                    for _ in 0..count {
                        let key = String::decode(input)?;
                        if last
                            .as_ref()
                            .is_some_and(|last| last.as_bytes() >= key.as_bytes())
                        {
                            return Err(CanonicalFault::UnsortedOrDuplicateKey.into());
                        }
                        let value = serde_json::Value::decode(input)?;
                        last = Some(key.clone());
                        entries.insert(key, value);
                    }
                    Self::Object(entries)
                }
            })
        })
    }
}

// Library-function definitions.

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
        out.seq(2);
        self.functions.encode(out);
        self.block.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            functions: BTreeMap::decode(input)?,
            block: Vec::decode(input)?,
        })
    }
}

impl TokenCodec for Implementation {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Native => out.sum(self.codec_variant(), 0),
            Self::Body(body) | Self::Both(body) => {
                out.sum(self.codec_variant(), 1);
                body.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| match variant {
                0 => Some(0),
                1 | 2 => Some(1),
                _ => None,
            })? {
                0 => Self::Native,
                1 => Self::Body(FunctionBody::decode(input)?),
                _ => Self::Both(FunctionBody::decode(input)?),
            },
        )
    }
}

/// Node kind 14's typed body.
impl TokenCodec for FunctionDefinition {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(8);
        out.unsigned(u64::from(self.kernel));
        self.name.encode(out);
        self.signature.encode(out);
        self.errors.encode(out);
        self.charge.encode(out);
        self.guard.encode(out);
        self.implementation.encode(out);
        out.unsigned(u64::from(self.native_version));
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(8)?;
        Ok(Self {
            kernel: input.unsigned_u32()?,
            name: QualifiedName::decode(input)?,
            signature: Signature::decode(input)?,
            errors: BTreeSet::decode(input)?,
            charge: Formula::decode(input)?,
            guard: Option::decode(input)?,
            implementation: Implementation::decode(input)?,
            native_version: input.unsigned_u32()?,
        })
    }
}
