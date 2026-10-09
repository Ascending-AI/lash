//! The six function definitions: names, signatures, error kinds, charges
//! and guards. Everything here is in a function's identity.

use std::collections::BTreeSet;

use lash_kernel_doc::{
    Formula, FunctionDefinition, FunctionName, Guard, Implementation, KERNEL_VERSION, Name,
    Operand, Param, RecordType, RecordTypeField, Signature, Type,
};

/// The `brand` field of every regex record.
pub const BRAND: &str = "regex.ecma";

/// The kind of the error raised for a pattern or flags that are refused.
pub const SYNTAX_ERROR: &str = "regex.syntax";

/// The kind of the error raised when a result would hold half of a
/// surrogate pair, which kernel text cannot.
pub const LONE_SURROGATE_ERROR: &str = "regex.lone_surrogate";

/// What the guard counts. A second implementation counts the same thing.
pub const GUARD_UNIT: &str =
    "one step of the backtracking matcher, or one UTF-16 code unit of text written to the result";

/// Guard units every matching call may spend whatever its input.
pub const GUARD_BASE: u64 = 1_000_000;

/// Guard units a matching call may spend per unit of its input's size.
pub const GUARD_PER_INPUT_UNIT: u64 = 64;

/// What a matching call is charged before its arguments are measured: the
/// price of [`GUARD_BASE`].
pub const CHARGE_BASE: u64 = 100;

/// What a call is charged per unit of the pattern's size: the compile,
/// charged on every call whether or not the engine still holds the
/// program.
pub const CHARGE_PER_PATTERN_UNIT: u64 = 8;

/// The six functions, by what they do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Operation {
    CompileCheck,
    Exec,
    Test,
    MatchAll,
    Replace,
    Split,
}

impl Operation {
    pub const ALL: [Self; 6] = [
        Self::CompileCheck,
        Self::Exec,
        Self::Test,
        Self::MatchAll,
        Self::Replace,
        Self::Split,
    ];

    /// The function's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::CompileCheck => "regex.ecma.compile_check",
            Self::Exec => "regex.ecma.exec",
            Self::Test => "regex.ecma.test",
            Self::MatchAll => "regex.ecma.match_all",
            Self::Replace => "regex.ecma.replace",
            Self::Split => "regex.ecma.split",
        }
    }

    /// The function's definition. Its identity is the hash of this value.
    #[expect(
        clippy::expect_used,
        reason = "every name is a literal of this file that is a qualified name"
    )]
    pub fn definition(self) -> FunctionDefinition {
        let (params, result) = match self {
            Self::CompileCheck => (
                vec![param("pattern", Type::Text), param("flags", Type::Text)],
                regex_type(),
            ),
            Self::Exec => (
                vec![param("regex", regex_type()), param("input", Type::Text)],
                record([
                    ("match", Type::Union(vec![Type::Null, match_type()])),
                    ("lastIndex", Type::Int),
                ]),
            ),
            Self::Test => (
                vec![param("regex", regex_type()), param("input", Type::Text)],
                record([("matched", Type::Bool), ("lastIndex", Type::Int)]),
            ),
            Self::MatchAll => (
                vec![param("regex", regex_type()), param("input", Type::Text)],
                Type::List(Box::new(match_type())),
            ),
            Self::Replace => (
                vec![
                    param("regex", regex_type()),
                    param("input", Type::Text),
                    param("replacement", Type::Text),
                ],
                record([("text", Type::Text), ("lastIndex", Type::Int)]),
            ),
            Self::Split => (
                vec![
                    param("regex", regex_type()),
                    param("input", Type::Text),
                    Param {
                        optional: true,
                        ..param("limit", Type::Int)
                    },
                ],
                Type::List(Box::new(text_or_null())),
            ),
        };
        let size = |name: &str| Formula::Size(Operand::Param(Name::new(name)));
        let (charge, guard, errors) = match self {
            Self::CompileCheck => (
                Formula::Sum(vec![
                    Formula::Constant(CHARGE_BASE),
                    Formula::Product(vec![
                        Formula::Constant(CHARGE_PER_PATTERN_UNIT),
                        size("pattern"),
                    ]),
                    size("flags"),
                ]),
                None,
                vec![SYNTAX_ERROR],
            ),
            _ => {
                let mut terms = vec![
                    Formula::Constant(CHARGE_BASE),
                    Formula::Product(vec![
                        Formula::Constant(CHARGE_PER_PATTERN_UNIT),
                        Formula::DeepSize(Operand::Param(Name::new("regex"))),
                    ]),
                    size("input"),
                ];
                if self == Self::Replace {
                    terms.push(size("replacement"));
                }
                terms.push(Formula::DeepSize(Operand::Result));
                let guard = Guard {
                    unit: GUARD_UNIT.to_string(),
                    limit: Formula::Sum(vec![
                        Formula::Constant(GUARD_BASE),
                        Formula::Product(vec![
                            Formula::Constant(GUARD_PER_INPUT_UNIT),
                            size("input"),
                        ]),
                    ]),
                };
                let errors = if self == Self::Test {
                    vec![SYNTAX_ERROR]
                } else {
                    vec![SYNTAX_ERROR, LONE_SURROGATE_ERROR]
                };
                (Formula::Sum(terms), Some(guard), errors)
            }
        };
        FunctionDefinition {
            kernel: KERNEL_VERSION,
            name: FunctionName::new(self.name()).expect("a qualified name"),
            signature: Signature { params, result },
            errors: errors
                .into_iter()
                .map(str::to_string)
                .collect::<BTreeSet<_>>(),
            charge,
            guard,
            implementation: Implementation::Native,
        }
    }
}

fn param(name: &str, ty: Type) -> Param {
    Param {
        name: Name::new(name),
        ty,
        optional: false,
    }
}

fn record<const N: usize>(fields: [(&str, Type); N]) -> Type {
    Type::Record(RecordType {
        fields: fields
            .into_iter()
            .map(|(name, ty)| RecordTypeField {
                name: name.to_string(),
                ty,
                optional: false,
            })
            .collect(),
        rest: None,
    })
}

fn text_or_null() -> Type {
    Type::Union(vec![Type::Text, Type::Null])
}

/// A regex: `{brand, pattern, flags, lastIndex}`.
fn regex_type() -> Type {
    record([
        ("brand", Type::Enum(vec![BRAND.to_string()])),
        ("pattern", Type::Text),
        ("flags", Type::Text),
        ("lastIndex", Type::Int),
    ])
}

/// One match: `{index, groups, named}`.
fn match_type() -> Type {
    record([
        ("index", Type::Int),
        ("groups", Type::List(Box::new(text_or_null()))),
        (
            "named",
            Type::Union(vec![
                Type::Null,
                Type::Record(RecordType {
                    fields: Vec::new(),
                    rest: Some(Box::new(text_or_null())),
                }),
            ]),
        ),
    ])
}
