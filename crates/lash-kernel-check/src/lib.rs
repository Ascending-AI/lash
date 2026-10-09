//! The lash kernel's checker.
//!
//! A document is the only authority (`K-DOC-001`). This crate reads one and
//! says what follows from it:
//!
//! - **Linking.** Every name is resolved to the declaration it means, scope
//!   by scope, and a closure's shared variables are found. A variable used
//!   where nothing has bound it is refused, naming the node.
//! - **Derived facts.** [`derive`] builds the [`Graph`]: the total typed
//!   read model a host shows and edits against, with node ids, edges,
//!   scopes, type facets, each function's effect set and the execution
//!   sites a machine reports and a parked run saves (`K-SITE`).
//! - **Admission.** [`admit`] checks the document against an
//!   [`Environment`], the effects and function identities an embedder
//!   provides, and refuses it with a typed [`Refusal`] that names everything
//!   missing or mismatched (`K-ADM`).
//!
//! Nothing here is stored: every fact is recomputed from the document. The
//! crate depends on `lash-kernel-doc` and on no other lash crate.

mod admit;
mod flow;
mod graph;
mod link;
mod manifest;
mod refusal;
mod types;
mod typing;

#[cfg(test)]
mod tests;

pub use admit::{Admitted, Environment, admit, derive, derive_definition};
pub use graph::{
    ActionNode, AtomNode, Binding, BindingId, BindingKind, CalleeNode, CatchNode, Edge, EdgeKind,
    EffectSet, ExecutionSite, ExprNode, Graph, GraphNode, MapEntryNode, MemberNode, NodeId,
    NodeKind, PlaceNode, RecordEntryNode, Reference, Scope, ScopeId, SiteKind, StmtNode, Target,
    UnitView,
};
pub use manifest::{Requirements, requirements};
pub use refusal::{Refusal, RefusalReason, Refused};
