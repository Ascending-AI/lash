//! Binary syntax and its admitted calls, in source-priority order.
use crate::adapter::BinaryOp;
use crate::types::Ty;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direct {
    Numeric(&'static str),
    NumericSwapped(&'static str),
    NumericEqual { negated: bool },
    Concat,
    ConcatFloat { float_left: bool },
}

#[derive(Clone, Copy)]
pub(crate) struct Binary {
    pub token: &'static str,
    pub helper: &'static str,
    pub swapped: bool,
    alternatives: &'static [Alternative],
}

#[derive(Clone, Copy)]
struct Alternative {
    call: Direct,
    inverse: &'static str,
    assert_operands: bool,
}

macro_rules! binary {
    ($source:ident, $token:literal, $helper:literal) => {
        Binary {
            token: $token,
            helper: $helper,
            swapped: false,
            alternatives: &[],
        }
    };
    ($source:ident, $token:literal, $helper:literal, swapped) => {
        Binary {
            swapped: true,
            ..binary!($source, $token, $helper)
        }
    };
    ($source:ident, $token:literal, $helper:literal, $direct:expr, $inverse:literal, $guard:literal, [$($extra:expr),*]) => {
        Binary { alternatives: &[Alternative { call: $direct, inverse: $inverse, assert_operands: $guard }, $($extra),*], ..binary!($source, $token, $helper) }
    };
    ($source:ident, $token:literal, $helper:literal, $direct:expr, $inverse:literal, $guard:literal) => {
        Binary {
            alternatives: &[Alternative { call: $direct, inverse: $inverse, assert_operands: $guard }],
            ..binary!($source, $token, $helper)
        }
    };
}

macro_rules! declarations {
    ($(($source:ident, $($definition:tt)*)),* $(,)?) => {
        pub(crate) const BINARY: &[Binary] = &[$(binary!($source, $($definition)*)),*];
        pub(crate) fn binary(op: BinaryOp) -> &'static Binary {
            match op { $(BinaryOp::$source => &binary!($source, $($definition)*),)* BinaryOp::InstanceOf => unreachable!("instanceof is resolved by its class") }
        }
    };
}
declarations! {
    (Add, "+", "ts.add", Direct::Numeric("num.add"), "+", false, [
        Alternative { call: Direct::Concat, inverse: "+", assert_operands: false },
        Alternative { call: Direct::ConcatFloat { float_left: true }, inverse: "+", assert_operands: false },
        Alternative { call: Direct::ConcatFloat { float_left: false }, inverse: "+", assert_operands: false }
    ]),
    (Subtract, "-", "ts.sub", Direct::Numeric("num.sub"), "-", false),
    (Multiply, "*", "ts.mul", Direct::Numeric("num.mul"), "*", false),
    (Divide, "/", "ts.div", Direct::Numeric("num.div"), "/", false),
    (Remainder, "%", "ts.rem", Direct::Numeric("num.rem_trunc"), "%", true),
    (Exponent, "**", "ts.pow"),
    (BitAnd, "&", "ts.bit_and"), (BitOr, "|", "ts.bit_or"), (BitXor, "^", "ts.bit_xor"),
    (ShiftLeft, "<<", "ts.shl"), (ShiftRight, ">>", "ts.shr"), (ShiftRightUnsigned, ">>>", "ts.ushr"),
    (StrictEqual, "===", "ts.strict_equals", Direct::NumericEqual { negated: false }, "===", true),
    (StrictNotEqual, "!==", "ts.strict_not_equals", Direct::NumericEqual { negated: true }, "!==", false),
    (LooseEqual, "==", "ts.loose_equals", Direct::NumericEqual { negated: false }, "===", true),
    (LooseNotEqual, "!=", "ts.loose_not_equals", Direct::NumericEqual { negated: true }, "!==", false),
    (Less, "<", "ts.lt", Direct::Numeric("num.lt"), "<", false),
    (LessEqual, "<=", "ts.le", Direct::Numeric("num.le"), "<=", false),
    (Greater, ">", "ts.gt", Direct::NumericSwapped("num.lt"), "<", false),
    (GreaterEqual, ">=", "ts.ge", Direct::NumericSwapped("num.le"), "<=", false),
    (In, "in", "ts.in", swapped),
}

impl Binary {
    pub fn direct(&self, left: &Ty, right: &Ty) -> Option<Direct> {
        self.alternatives
            .iter()
            .find(|entry| entry.call.admits(left, right))
            .map(|entry| entry.call)
    }
}

pub(crate) fn native_inverse(name: &str) -> Option<&'static str> {
    BINARY.iter().find_map(|entry| {
        entry
            .alternatives
            .iter()
            .find(|alternative| alternative.call.function() == name)
            .map(|alternative| alternative.inverse)
            .or_else(|| (entry.helper == name).then_some(entry.token))
    })
}
pub(crate) fn asserted(token: &str) -> bool {
    BINARY
        .iter()
        .flat_map(|entry| entry.alternatives)
        .any(|entry| entry.inverse == token && entry.assert_operands)
}
pub(crate) fn mirror(token: &str) -> Option<&'static str> {
    let source = BINARY.iter().find(|entry| entry.token == token)?;
    let call = source.alternatives.first()?.call;
    let mirrored = match call {
        Direct::Numeric(function) => Direct::NumericSwapped(function),
        Direct::NumericSwapped(function) => Direct::Numeric(function),
        _ => return None,
    };
    BINARY
        .iter()
        .find(|entry| {
            entry
                .alternatives
                .iter()
                .any(|alternative| alternative.call == mirrored)
        })
        .map(|entry| entry.token)
}

impl Direct {
    fn admits(self, left: &Ty, right: &Ty) -> bool {
        match self {
            Self::Numeric(_) | Self::NumericSwapped(_) | Self::NumericEqual { .. } => {
                left.is_number() && right.is_number()
            }
            Self::Concat => *left == Ty::Text && *right == Ty::Text,
            Self::ConcatFloat { float_left: true } => *left == Ty::Float && *right == Ty::Text,
            Self::ConcatFloat { float_left: false } => *left == Ty::Text && *right == Ty::Float,
        }
    }
    pub fn function(self) -> &'static str {
        match self {
            Self::Numeric(function) | Self::NumericSwapped(function) => function,
            Self::NumericEqual { .. } => "eq",
            Self::Concat | Self::ConcatFloat { .. } => "text.concat",
        }
    }
}

pub(crate) const NUMBER_GUARD: &str = "ts.number.checked";

#[derive(Clone, Copy)]
pub(crate) enum UnaryCall {
    Invoke(&'static str),
    Discard,
}
#[derive(Clone, Copy)]
pub(crate) enum UnaryNative {
    NumberIdentity,
    NumberNegation(&'static str),
    Boolean(&'static str),
    None,
}
#[derive(Clone, Copy)]
pub(crate) struct Unary {
    pub token: &'static str,
    pub call: UnaryCall,
    pub native: UnaryNative,
}
macro_rules! unary_declarations {
    ($(($source:ident, $token:literal, $call:expr, $native:expr)),* $(,)?) => {
        const UNARY: &[Unary] = &[$(Unary { token: $token, call: $call, native: $native }),*];
        pub(crate) fn unary(op: crate::adapter::UnaryOp) -> &'static Unary {
            match op { $(crate::adapter::UnaryOp::$source => &Unary { token: $token, call: $call, native: $native }),* }
        }
    };
}
unary_declarations! {
    (Plus, "+", UnaryCall::Invoke("ts.to_number"), UnaryNative::NumberIdentity),
    (Minus, "-", UnaryCall::Invoke("ts.neg"), UnaryNative::NumberNegation("num.neg")),
    (Not, "!", UnaryCall::Invoke("ts.not"), UnaryNative::Boolean("bool.not")),
    (TypeOf, "typeof ", UnaryCall::Invoke("ts.typeof"), UnaryNative::None),
    (BitNot, "~", UnaryCall::Invoke("ts.bit_not"), UnaryNative::None),
    (Void, "void ", UnaryCall::Discard, UnaryNative::None),
}
pub(crate) fn unary_inverse(helper: &str, native: bool) -> Option<(&'static str, bool)> {
    UNARY.iter().find_map(|entry| {
        if native {
            match entry.native {
                UnaryNative::NumberNegation(name) if name == helper => Some((entry.token, false)),
                UnaryNative::Boolean(name) if name == helper => Some((entry.token, true)),
                _ => None,
            }
        } else {
            matches!(entry.call, UnaryCall::Invoke(name) if name == helper)
                .then_some((entry.token, false))
        }
    })
}
pub(crate) fn native_not() -> &'static str {
    match unary(crate::adapter::UnaryOp::Not).native {
        UnaryNative::Boolean(name) => name,
        _ => unreachable!("boolean negation has a native alternative"),
    }
}
