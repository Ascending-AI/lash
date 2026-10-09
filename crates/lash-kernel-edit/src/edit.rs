//! The edit vocabulary: every change a host makes to a document, as data.
//!
//! An edit names nodes by their [`Site`] in the transaction's base document
//! (`K-EDIT-002`). Its payloads are kernel forms; nothing here is source
//! text and nothing names a dialect.

use std::collections::BTreeSet;

use lash_kernel_doc::{
    Action, Atom, Block, Catch, DocumentId, EffectName, Expr, Function, FunctionId, Label, Name,
    NumberPolicy, Signature, Site, Stmt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A set of edits applied whole or not at all (`K-EDIT-001`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Transaction {
    /// The identity of the document the edits were written against. Every
    /// site in `edits` is a site of that document.
    pub base: DocumentId,
    /// Applied in order.
    pub edits: Vec<Edit>,
}

impl Transaction {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// Where a statement is put: in `block`, ahead of the statement `before`,
/// or at the block's end.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub block: Site,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Site>,
}

impl Position {
    /// The end of `block`.
    pub fn end(block: Site) -> Self {
        Self {
            block,
            before: None,
        }
    }

    /// Ahead of the statement `before`, in the block that holds it.
    ///
    /// Returns `None` when `before` is a unit's body, which no block holds.
    pub fn before(before: Site) -> Option<Self> {
        let mut block = before.clone();
        block.path.pop()?;
        Some(Self {
            block,
            before: Some(before),
        })
    }
}

/// One typed change to a document or to its annotations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Edit {
    InsertStatement {
        at: Position,
        statement: Stmt,
    },
    /// Removes a statement and everything under it.
    RemoveStatement {
        statement: Site,
    },
    /// Moves a statement and everything under it. Every node moved
    /// survives.
    MoveStatement {
        statement: Site,
        to: Position,
    },
    /// Copies a statement and everything under it, annotations included.
    /// The copy is new: no node of it corresponds to a node of the base.
    CloneStatement {
        statement: Site,
        to: Position,
    },
    /// Replaces a whole statement. The statement survives; what was under
    /// it does not.
    ReplaceStatement {
        statement: Site,
        with: Stmt,
    },
    /// Replaces an expression, at any depth. The expression survives; what
    /// was under it does not.
    ReplaceExpression {
        expression: Site,
        with: Expr,
    },
    /// Replaces an action: the right-hand side that calls, performs, waits,
    /// spawns or cancels.
    ReplaceAction {
        action: Site,
        with: Action,
    },
    /// Replaces one argument of an action, counted from 0 in the order the
    /// action writes them. A `sleep`, `join`, `join` of a list and `cancel`
    /// have the one argument.
    SetArgument {
        action: Site,
        index: u32,
        argument: Atom,
    },
    /// Replaces the condition of an `if` or a `while`.
    SetCondition {
        statement: Site,
        condition: Expr,
    },
    /// Sets or clears the `catch` of a `try`. The `try`, its body and its
    /// `finally` survive.
    SetCatch {
        statement: Site,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        catch: Option<Catch>,
    },
    /// Sets or clears the `finally` of a `try`. The `try`, its body and its
    /// `catch` survive.
    SetFinally {
        statement: Site,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finally: Option<Block>,
    },
    /// Renames a variable where it is declared and in every use that
    /// resolves to that declaration.
    ///
    /// `declared_at` is the node that declares it: the `let`, the `for`,
    /// the `try` of its `catch`, the closure expression, or a declared
    /// function's body for one of its parameters.
    RenameVariable {
        declared_at: Site,
        name: Name,
        to: Name,
    },
    /// Sets or clears a node's label.
    SetLabel {
        node: Site,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<Label>,
    },
    /// Sets or clears one entry of what a host keeps on a node.
    SetData {
        node: Site,
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<serde_json::Value>,
    },
    InsertFunction {
        name: Name,
        function: Function,
    },
    /// Removes a declared function, and its entry when it has one.
    RemoveFunction {
        name: Name,
    },
    /// Replaces a declared function's parameters and body. Its body block
    /// survives; what was under it does not.
    ReplaceFunction {
        name: Name,
        function: Function,
    },
    /// Renames a declared function in its declaration, its entry and every
    /// call, spawn and reference of it.
    RenameFunction {
        from: Name,
        to: Name,
    },
    /// Sets which of `main`'s top-level variables are the program's own
    /// (`K-SES-001`).
    SetPrivateBindings {
        names: BTreeSet<Name>,
    },
    /// Makes a declared function one a host may start, under `signature`.
    InsertEntry {
        function: Name,
        signature: Signature,
    },
    /// Makes a declared function no longer one a host may start. The
    /// function stays.
    RemoveEntry {
        function: Name,
    },
    /// [`Edit::RenameFunction`] of a function that is an entry.
    RenameEntry {
        from: Name,
        to: Name,
    },
    SetEntrySignature {
        function: Name,
        signature: Signature,
    },
    /// Sets the signature the document expects of an effect it performs.
    SetEffectSignature {
        effect: EffectName,
        signature: Signature,
    },
    /// Sets how an effect result's bare number decodes (`K-EFF-006`).
    SetNumberPolicy {
        numbers: NumberPolicy,
    },
    /// Adopts a corrected library function: every call of `from` in the
    /// document becomes a call of `to` (`K-VER-002`).
    ReplaceFunctionIdentity {
        from: FunctionId,
        to: FunctionId,
    },
}
