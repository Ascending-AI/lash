//! The names of the typed workflow edits, for coverage laws.

use crate::WorkflowEdit;

/// The name of `edit`'s kind. The match has no wildcard arm, so a new edit
/// does not compile until it is named here, and a law that compares the
/// edits it applied with [`WORKFLOW_EDIT_KINDS`] then fails until it applies
/// the new one.
pub fn workflow_edit_kind(edit: &WorkflowEdit) -> &'static str {
    match edit {
        WorkflowEdit::InsertNode { .. } => "InsertNode",
        WorkflowEdit::CloneNode { .. } => "CloneNode",
        WorkflowEdit::RemoveNode { .. } => "RemoveNode",
        WorkflowEdit::MoveNode { .. } => "MoveNode",
        WorkflowEdit::ReplaceNode { .. } => "ReplaceNode",
        WorkflowEdit::ReplaceExpression { .. } => "ReplaceExpression",
        WorkflowEdit::SetBinding { .. } => "SetBinding",
        WorkflowEdit::RenameBinding { .. } => "RenameBinding",
        WorkflowEdit::SetCondition { .. } => "SetCondition",
        WorkflowEdit::SetLabel { .. } => "SetLabel",
        WorkflowEdit::SetLoopBinding { .. } => "SetLoopBinding",
        WorkflowEdit::SetCatch { .. } => "SetCatch",
        WorkflowEdit::SetFinally { .. } => "SetFinally",
        WorkflowEdit::SetBodyLayout { .. } => "SetBodyLayout",
        WorkflowEdit::InsertProcess { .. } => "InsertProcess",
        WorkflowEdit::RemoveProcess { .. } => "RemoveProcess",
        WorkflowEdit::RenameProcess { .. } => "RenameProcess",
        WorkflowEdit::SetProcessSignature { .. } => "SetProcessSignature",
        WorkflowEdit::SetProcessWrapper { .. } => "SetProcessWrapper",
        WorkflowEdit::InsertFunction { .. } => "InsertFunction",
        WorkflowEdit::ReplaceFunction { .. } => "ReplaceFunction",
        WorkflowEdit::RemoveFunction { .. } => "RemoveFunction",
        WorkflowEdit::SetPrivateBindings { .. } => "SetPrivateBindings",
    }
}

/// The names [`workflow_edit_kind`] answers, one per [`WorkflowEdit`]
/// variant.
pub const WORKFLOW_EDIT_KINDS: [&str; 23] = [
    "InsertNode",
    "CloneNode",
    "RemoveNode",
    "MoveNode",
    "ReplaceNode",
    "ReplaceExpression",
    "SetBinding",
    "RenameBinding",
    "SetCondition",
    "SetLabel",
    "SetLoopBinding",
    "SetCatch",
    "SetFinally",
    "SetBodyLayout",
    "InsertProcess",
    "RemoveProcess",
    "RenameProcess",
    "SetProcessSignature",
    "SetProcessWrapper",
    "InsertFunction",
    "ReplaceFunction",
    "RemoveFunction",
    "SetPrivateBindings",
];
