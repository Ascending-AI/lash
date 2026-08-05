use std::borrow::Borrow;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::artifact::{
    HostRequirements, ModuleArtifact, host_requirements_for_program_with_catalog,
};
use crate::ast::{
    AssignPathStep, AstString, Declaration, Expr, ListComprehensionClause, ProcessDecl,
    ProcessParam, Program, ResourceRefExpr, TypeExpr, TypeField, format_type_expr,
};
use crate::lexer::Span;

include!("linker/catalog.rs");
include!("linker/host.rs");
include!("linker/errors.rs");
include!("linker/pass_setup.rs");
include!("linker/lower_expr.rs");
include!("linker/pass_validation.rs");
include!("linker/type_helpers.rs");
include!("linker/facets.rs");
include!("linker/tests.rs");

#[derive(Clone, Debug, Default)]
pub(crate) struct WorkflowLinkAnalysis {
    nodes: BTreeMap<usize, WorkflowLinkNodeFacts>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct WorkflowLinkNodeFacts {
    pub(crate) available_variables: BTreeMap<String, TypeExpr>,
    pub(crate) expected_arguments: Vec<WorkflowLinkExpectedArgument>,
    pub(crate) diagnostics: Vec<LinkError>,
}

#[derive(Clone, Debug)]
pub(crate) struct WorkflowLinkExpectedArgument {
    pub(crate) slot: String,
    pub(crate) ty: TypeExpr,
}

#[derive(Debug, Default)]
struct ExpectedTypeFacts {
    by_expression: BTreeMap<usize, TypeExpr>,
}
