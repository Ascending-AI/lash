//! The explicit codec variant numbers of the document's enums (FIG-5887 §2).
//!
//! Each number is the variant's zero-based declaration position at revision
//! `9503e220ed`, transcribed once into an exhaustive match. From here on the
//! numbers are the format: a new variant takes an unused number, and no
//! variant is renumbered when the Rust declaration moves. The binary codecs
//! read these tables; nothing derives a number from declaration order again.

use crate::graph_tokens::CodecVariant;
use crate::{
    Action, Atom, Callee, Datum, Expr, Formula, Identity, Implementation, JoinMode, Literal,
    Measure, Member, NumberPolicy, Object, Operand, Place, Rhs, Stmt, TaskIdentity, Type, Unit,
    Value,
};

#[doc(hidden)]
impl CodecVariant for Value {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Null => 0,
            Self::Absent => 1,
            Self::Bool(..) => 2,
            Self::Int(..) => 3,
            Self::Float(..) => 4,
            Self::Text(..) => 5,
            Self::Bytes(..) => 6,
            Self::Timestamp(..) => 7,
            Self::Tuple(..) => 8,
            Self::List(..) => 9,
            Self::Map(..) => 10,
            Self::Set(..) => 11,
            Self::Record(..) => 12,
            Self::Closure(..) => 13,
            Self::Error(..) => 14,
            Self::Task(..) => 15,
            Self::Function(..) => 16,
            Self::Handle(..) => 17,
            Self::Ref(..) => 18,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Datum {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Null => 0,
            Self::Absent => 1,
            Self::Bool(..) => 2,
            Self::Int(..) => 3,
            Self::Float(..) => 4,
            Self::Number(..) => 5,
            Self::Text(..) => 6,
            Self::Bytes(..) => 7,
            Self::Timestamp(..) => 8,
            Self::Tuple(..) => 9,
            Self::List(..) => 10,
            Self::Map(..) => 11,
            Self::Set(..) => 12,
            Self::Record(..) => 13,
            Self::Error(..) => 14,
            Self::Function(..) => 15,
            Self::Handle(..) => 16,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Object {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::List(..) => 0,
            Self::Map(..) => 1,
            Self::Set(..) => 2,
            Self::Record(..) => 3,
            Self::Closure(..) => 4,
            Self::Variable(..) => 5,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Identity {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Object(..) => 0,
            Self::Task(..) => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Stmt {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Let { .. } => 0,
            Self::Assign { .. } => 1,
            Self::Remove { .. } => 2,
            Self::Do { .. } => 3,
            Self::If { .. } => 4,
            Self::For { .. } => 5,
            Self::While { .. } => 6,
            Self::Break => 7,
            Self::Continue => 8,
            Self::Return { .. } => 9,
            Self::Try(..) => 10,
            Self::Throw { .. } => 11,
            Self::Print { .. } => 12,
            Self::Finish { .. } => 13,
            Self::Fail { .. } => 14,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Rhs {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Expr(..) => 0,
            Self::Action(..) => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Action {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Call { .. } => 0,
            Self::Perform { .. } => 1,
            Self::Sleep { .. } => 2,
            Self::Join { .. } => 3,
            Self::JoinMany { .. } => 4,
            Self::Yield => 5,
            Self::Spawn { .. } => 6,
            Self::Cancel { .. } => 7,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for JoinMode {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::All => 0,
            Self::AllSettled => 1,
            Self::Race => 2,
            Self::Any => 3,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Callee {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Declared(..) => 0,
            Self::Value(..) => 1,
            Self::Library(..) => 2,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Atom {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Variable(..) => 0,
            Self::Literal(..) => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Literal {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Null => 0,
            Self::Absent => 1,
            Self::Bool(..) => 2,
            Self::Int(..) => 3,
            Self::Float(..) => 4,
            Self::Text(..) => 5,
            Self::Bytes(..) => 6,
            Self::Function(..) => 7,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Expr {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Literal(..) => 0,
            Self::Variable(..) => 1,
            Self::Tuple(..) => 2,
            Self::List(..) => 3,
            Self::Map(..) => 4,
            Self::Set(..) => 5,
            Self::Record(..) => 6,
            Self::Member(..) => 7,
            Self::Closure(..) => 8,
            Self::Call { .. } => 9,
            Self::Clock => 10,
            Self::Random => 11,
            Self::Read(..) => 12,
            Self::If { .. } => 13,
            Self::Let { .. } => 14,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Member {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Field { .. } => 0,
            Self::Index { .. } => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Place {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Variable(..) => 0,
            Self::Member(..) => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Unit {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Main => 0,
            Self::Function(..) => 1,
            Self::Library(..) => 2,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Type {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Any => 0,
            Self::Null => 1,
            Self::Absent => 2,
            Self::Bool => 3,
            Self::Int => 4,
            Self::Float => 5,
            Self::Number => 6,
            Self::Text => 7,
            Self::Bytes => 8,
            Self::Timestamp => 9,
            Self::Tuple(..) => 10,
            Self::List(..) => 11,
            Self::Map(..) => 12,
            Self::Set(..) => 13,
            Self::Record(..) => 14,
            Self::Enum(..) => 15,
            Self::Function(..) => 16,
            Self::Task(..) => 17,
            Self::Error => 18,
            Self::Handle(..) => 19,
            Self::Union(..) => 20,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Implementation {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Native => 0,
            Self::Body(..) => 1,
            Self::Both(..) => 2,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Formula {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Constant(..) => 0,
            Self::Size(..) => 1,
            Self::DeepSize(..) => 2,
            Self::NestedSize(..) => 3,
            Self::Magnitude(..) => 4,
            Self::Sum(..) => 5,
            Self::Product(..) => 6,
            Self::Max(..) => 7,
            Self::Min(..) => 8,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Operand {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Param(..) => 0,
            Self::Result => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Measure {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Size => 0,
            Self::DeepSize => 1,
            Self::NestedSize => 2,
            Self::Magnitude => 3,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for TaskIdentity {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Main => 0,
            Self::Spawned(..) => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for NumberPolicy {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Float => 0,
            Self::BySpelling => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for crate::SourceLanguage {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::TypeScript => 0,
            Self::Python => 1,
        }
    }
}
