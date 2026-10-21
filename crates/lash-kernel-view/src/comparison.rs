//! A condition read as the comparison its package declares.
//! The supplied package view owns each declaration; no dialect name appears here.

use lash_kernel_doc::Literal;

use lash_vocabulary::sites::Site;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A condition that is one comparison of values a host can name. A binary
/// comparison reads `left` then `right`; `contains` holds when `left`
/// holds `right`, `starts_with` and `ends_with` when `left` begins or ends
/// with `right`. Each holds under its dialect's own equality, order and
/// truth: TypeScript's `===` and Python's `==` both read as `eq`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ComparisonView {
    Eq {
        left: ValueRef,
        right: ValueRef,
    },
    Ne {
        left: ValueRef,
        right: ValueRef,
    },
    Lt {
        left: ValueRef,
        right: ValueRef,
    },
    Le {
        left: ValueRef,
        right: ValueRef,
    },
    Gt {
        left: ValueRef,
        right: ValueRef,
    },
    Ge {
        left: ValueRef,
        right: ValueRef,
    },
    Contains {
        left: ValueRef,
        right: ValueRef,
    },
    StartsWith {
        left: ValueRef,
        right: ValueRef,
    },
    EndsWith {
        left: ValueRef,
        right: ValueRef,
    },
    /// The condition holds when its operand is false (falsy, in a dialect
    /// with truthiness).
    Not {
        operand: ValueRef,
    },
}

/// A value a comparison reads: where it comes from, then the record fields
/// read from it, outermost first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValueRef {
    pub source: ValueSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
}

/// Where a compared value comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueSource {
    /// A parameter nothing assigns, by its binding in `dataflow`.
    Parameter {
        binding: usize,
    },
    Literal {
        value: Literal,
    },
    /// What the action at `site` yielded, held by a variable nothing
    /// reassigns.
    Result {
        site: Site,
    },
}
