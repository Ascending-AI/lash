//! Typed document validation failures shared by readers and host boundaries.
use crate::sites::Site;
use crate::{FunctionId, FunctionName, ImportSlot, Name};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Why a document or a definition is refused, and the node at fault.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[error("{reason}{}", at(.site))]
pub struct Invalid {
    /// The node at fault. `None` when the fault is in no node: the
    /// manifest, an entry, a signature.
    pub site: Option<Site>,
    pub reason: Box<InvalidReason>,
}

fn at(site: &Option<Site>) -> String {
    site.as_ref()
        .map(|site| format!(" (at {site})"))
        .unwrap_or_default()
}

/// An action form, as a refusal names one the body of a function with a
/// native implementation may not run (`K-LIB-004`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StatementForm {
    Call,
    Perform,
    Join,
    Yield,
    Spawn,
    Cancel,
    Await,
}

impl StatementForm {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Perform => "perform",
            Self::Join => "join",
            Self::Yield => "yield",
            Self::Spawn => "spawn",
            Self::Cancel => "cancel",
            Self::Await => "await",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum InvalidReason {
    #[error("invalid document: {message}")]
    Document { message: String },
    #[error("Import slot {slot:?} is missing or has the wrong kind")]
    Import { slot: ImportSlot },
    #[error("written for kernel version {found}; this reader supports version {supported}")]
    KernelVersion { found: u32, supported: u32 },
    #[error("nests deeper than {limit} levels; flatten the program")]
    TooDeep { limit: usize },
    #[error("a name is empty")]
    EmptyName,
    #[error("parameter `{name}` is declared twice")]
    DuplicateParam { name: Name },
    #[error("field `{field}` is written twice in one record")]
    DuplicateField { field: String },
    #[error("`{keyword}` is used outside a loop")]
    LoopControlOutsideLoop { keyword: Cow<'static, str> },
    /// A library call written as an expression's own call names a
    /// function with no native implementation: a call of it is an
    /// action, `invoke`.
    #[error("`{name}` has no native implementation, so a call to it is an `invoke` action")]
    CallNotNative {
        function: FunctionId,
        name: FunctionName,
    },
    #[error(
        "library function `{name}` ({function}) is written for kernel version {written_for}; \
         what lists it runs under version {required}"
    )]
    FunctionOfAnotherVersion {
        function: FunctionId,
        name: FunctionName,
        written_for: u32,
        required: u32,
    },
    #[error("library function {function} is called but not listed in the manifest")]
    FunctionNotListed { function: FunctionId },
    #[error(
        "library function {function} has no definition here, so a call of it cannot be checked"
    )]
    FunctionNotResolved { function: FunctionId },
    #[error("helper {function} is unavailable: {reason}")]
    HelperUnavailable {
        function: FunctionId,
        reason: HelperUnavailableReason,
    },
    #[error("`{name}` takes at most {accepted} argument(s) and at least {required}; {given} given")]
    CallArity {
        name: String,
        given: usize,
        required: usize,
        accepted: usize,
    },
    #[error("effect `{slot:?}` is performed but not listed in the manifest")]
    EffectNotListed { slot: ImportSlot },
    #[error("function `{name}` is not declared")]
    FunctionNotDeclared { name: Name },
    #[error("a library function's body cannot name a declared function (`{name}`)")]
    DeclaredFunctionInLibrary { name: Name },
    /// `K-LIB-004`: a library body performs only the operations its
    /// `effects` lists.
    #[error("effect `{slot:?}` is performed but not listed in the body's `effects`")]
    EffectNotDeclared { slot: ImportSlot },
    #[error("entry `{entry}` names no declared function")]
    EntryNotDeclared { entry: Name },
    #[error(
        "entry `{entry}` has {signature} parameter(s) in its signature and {declared} in its function"
    )]
    EntryArity {
        entry: Name,
        signature: usize,
        declared: usize,
    },
    #[error("invalid type: {problem}")]
    InvalidType { problem: Cow<'static, str> },
    #[error("a required parameter (`{name}`) follows an optional one")]
    RequiredAfterOptional { name: Name },
    #[error("a formula names `{name}`, which is not a parameter")]
    FormulaUnknownParam { name: Name },
    #[error("a guard's limit is known before the call runs, so it cannot measure the result")]
    LimitMeasuresResult,
    #[error("`max` and `min` need at least one amount")]
    EmptyFormula,
    #[error("a guard bounds a native implementation; this function states none")]
    GuardWithoutNative,
    /// `K-LIB-011`: a native version versions a native implementation.
    #[error("native version {version} is stated by a function with no native implementation")]
    NativeVersionWithoutNative { version: u32 },
    /// `K-LIB-011`: native versions count from 1.
    #[error("native version 0 is not a version; the first is 1")]
    NativeVersionZero,
    #[error("a guard's unit is empty")]
    EmptyGuardUnit,
    /// `K-LIB-003`: a function with a native implementation takes no
    /// function.
    #[error(
        "parameter `{name}` takes a function, so the function cannot have a native implementation"
    )]
    NativeTakesFunction { name: Name },
    /// `K-LIB-004`: a body that stands beside a native implementation
    /// cannot wait.
    #[error("the body stands beside a native implementation, so it cannot `{}`", .form.keyword())]
    NativeBodyMayWait { form: StatementForm },
    /// `K-EVAL-003`: an assignment evaluates its place before its
    /// right-hand side only when it writes a member, and that right-hand
    /// side is an expression, any action in it nested.
    #[error("a place-first assignment {problem}")]
    PlaceFirstAssignment { problem: Cow<'static, str> },
    /// A right-hand side that is one action with variable and literal
    /// arguments is the statement's action, not an expression: the one
    /// spelling kernel text reads back.
    #[error(
        "an action that is the whole right-hand side with atom arguments is the statement's action"
    )]
    ActionAsExpression,
    /// `K-CLO-004`: a direct start passes the signature's arguments as the
    /// function's parameters, one for one.
    #[error("a start adapter's signature has {signature} parameter(s) and its function {declared}")]
    StartArity { signature: usize, declared: usize },
    /// `K-CLO-004`: a token holds its tag first and its callable after it.
    #[error("a start adapter's token has an empty tag or holds its callable in slot 0")]
    StartToken,
}

/// Why an exact helper cannot execute on this node.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
    thiserror::Error,
)]
#[serde(rename_all = "snake_case")]
pub enum HelperUnavailableReason {
    #[error("retired")]
    Retired,
    #[error("unknown")]
    Unknown,
    #[error("corrupt")]
    Corrupt,
}
