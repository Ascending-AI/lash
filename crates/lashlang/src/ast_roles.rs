//! Language-neutral structural roles (FIG-3571).
//!
//! A front end marks the IR it generates with these roles so that no consumer
//! has to infer structure from a binding's spelling. Each role names its shape,
//! and [`super::validate_ast`] refuses a role whose expression lacks it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use std::collections::BTreeSet;

use super::{
    AssignPathStep, AstPath, AstString, BinaryOp, CatchClause, Declaration, Expr, InvalidAst,
    JavaScriptBinaryOp, Program, TryExpr,
};

/// The front end a [`Program`] was lowered from, recorded per artifact.
///
/// A front end names itself; the IR, the VM and every structural consumer are
/// the same whichever name it is. [`SourceLanguage::ir`] names a program
/// authored directly as IR.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct SourceLanguage(AstString);

impl SourceLanguage {
    pub fn new(name: impl Into<AstString>) -> Self {
        Self(name.into())
    }

    /// A program authored directly as IR, with no front end.
    pub fn ir() -> Self {
        Self::new("lashlang")
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// The origin of a [`super::ProcessDecl`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum ProcessOrigin {
    /// Authored as a module declaration.
    #[default]
    Declared,
    /// Lifted by the linker from the inline process literal at `site`, the
    /// literal's path in the program before lifting. The lifted declaration's
    /// name digests that literal's body and site
    /// ([`super::lifted_process_identity`]). Its last `hidden_params`
    /// parameters are the literal's hidden start arguments, not authored
    /// parameters.
    Lifted { site: AstPath, hidden_params: u32 },
}

impl ProcessOrigin {
    pub fn is_declared(&self) -> bool {
        matches!(self, Self::Declared)
    }

    pub fn is_lifted(&self) -> bool {
        matches!(self, Self::Lifted { .. })
    }
}

/// The structural roles a front end marks its generated IR with.
///
/// Each role is language-neutral: it names what a shape does, not the source
/// construct that produced it, and each front end chooses which of its
/// constructs lower to which role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum StructuralRole {
    /// An authored nested statement scope. `expr` is a `Block` whose every
    /// element is a statement.
    Scope,
    /// A statement list closed by a generated completion value. `expr` is a
    /// non-empty `Block`; every element but the last is a statement, and the
    /// last is the pure completion value the list evaluates to.
    Completion,
    /// A member assignment `root.path = value` that pins its reference base
    /// before evaluating the value. `expr` is `Block([base = object,
    /// (key = index)?, result = value, base.step = result, result])`, where
    /// `step` reads `key` when present. The value reads the pinned base only
    /// as the left operand of an arithmetic update (`base.step op operand`,
    /// the compound `object.step op= operand`); see [`AttributeAssignParts`].
    AttributeAssign,
    /// A callback-driven collection transform (`map`, `filter`, ...). `expr`
    /// is `Block([receiver = r, callback = f, operand = x*, driver =
    /// function, result+])`: it binds the receiver, then the callback, then
    /// any further operands (an initial value, evaluated extra arguments),
    /// then a driver function that captures the receiver and the callback,
    /// and ends with the one or two expressions that run the driver and yield
    /// the transform's result. `operation` is the front end's name for the
    /// transform; structure never depends on it. See
    /// [`CollectionTransformParts`].
    CollectionTransform { operation: AstString },
    /// The process failure wrapper around an authored run body. `expr` is
    /// `Try { body: Finish(Call { function: run, args }), catch e: Fail(e) }`,
    /// where `run` is a `Function` or a builtin call whose first argument is
    /// one; the run function's body is the authored process body.
    ProcessWrapper,
}

pub(super) fn check_program_roles(program: &Program) -> Result<(), InvalidAst> {
    let mut pending = vec![&program.main];
    for declaration in &program.declarations {
        match declaration {
            Declaration::Process(process) => pending.push(&process.body),
            Declaration::Function(function) => pending.push(&function.body),
            Declaration::Type(_) => {}
        }
    }
    while let Some(expr) = pending.pop() {
        if let Expr::Role { role, expr } = expr {
            role.check_shape(expr)?;
        }
        pending.extend(expr.children());
    }
    Ok(())
}

impl StructuralRole {
    /// The role's name as diagnostics and hashes spell it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Scope => "scope",
            Self::Completion => "completion",
            Self::AttributeAssign => "attribute_assign",
            Self::CollectionTransform { .. } => "collection_transform",
            Self::ProcessWrapper => "process_wrapper",
        }
    }

    /// Refuses `expr` unless it has this role's shape.
    pub fn check_shape(&self, expr: &Expr) -> Result<(), InvalidAst> {
        let malformed = |reason| InvalidAst::MalformedRole {
            role: self.name(),
            reason,
        };
        match self {
            Self::Scope => match expr {
                Expr::Block(_) => Ok(()),
                _ => Err(malformed("a scope wraps a block")),
            },
            Self::Completion => match expr {
                Expr::Block(items) if items.last().is_some_and(crate::is_pure_expr) => Ok(()),
                Expr::Block(_) => Err(malformed("a completion block ends with a pure value")),
                _ => Err(malformed("a completion wraps a block")),
            },
            Self::AttributeAssign => AttributeAssignParts::of(expr)
                .map(|_| ())
                .ok_or_else(|| malformed("an attribute assignment pins a base, evaluates a value, stores through the base and yields the value")),
            Self::CollectionTransform { .. } => CollectionTransformParts::of(expr)
                .map(|_| ())
                .ok_or_else(|| malformed(
                    "a collection transform binds its receiver, its callback and its operands, then a driver capturing both, then runs it",
                )),
            Self::ProcessWrapper => process_wrapper_run_path(expr)
                .map(|_| ())
                .ok_or_else(|| malformed("a process wrapper finishes with its run call and fails with what it catches")),
        }
    }
}

/// The authored parts of a [`StructuralRole::AttributeAssign`] block.
#[derive(Clone, Copy, Debug)]
pub struct AttributeAssignParts<'a> {
    /// The object whose attribute is written.
    pub object: &'a Expr,
    /// The written attribute: a field name, or an index expression.
    pub step: AttributeStep<'a>,
    /// The assigned value.
    pub value: &'a Expr,
    /// The [`Expr::children`] index of `value` inside the role's block.
    pub value_index: u32,
    /// Set when the value updates the current attribute by one arithmetic
    /// operator: `value` is `base.step op operand`.
    pub update: Option<AttributeUpdate<'a>>,
}

/// A compound attribute assignment's operator and right operand.
#[derive(Clone, Copy, Debug)]
pub struct AttributeUpdate<'a> {
    pub operator: UpdateOperator,
    pub operand: &'a Expr,
}

/// An arithmetic operator a compound attribute assignment applies to the
/// attribute's current value. Named neutrally: a front end's IR decides
/// whether the operation is Lashlang's or ECMA-262's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

impl UpdateOperator {
    fn of_javascript(op: JavaScriptBinaryOp) -> Option<Self> {
        Some(match op {
            JavaScriptBinaryOp::Add => Self::Add,
            JavaScriptBinaryOp::Subtract => Self::Subtract,
            JavaScriptBinaryOp::Multiply => Self::Multiply,
            JavaScriptBinaryOp::Divide => Self::Divide,
            JavaScriptBinaryOp::Remainder => Self::Remainder,
            _ => return None,
        })
    }

    fn of_lashlang(op: BinaryOp) -> Option<Self> {
        Some(match op {
            BinaryOp::Add => Self::Add,
            BinaryOp::Subtract => Self::Subtract,
            BinaryOp::Multiply => Self::Multiply,
            BinaryOp::Divide => Self::Divide,
            BinaryOp::Modulo => Self::Remainder,
            _ => return None,
        })
    }

    /// The ECMA-262 operator this update applies.
    pub fn javascript_op(self) -> JavaScriptBinaryOp {
        match self {
            Self::Add => JavaScriptBinaryOp::Add,
            Self::Subtract => JavaScriptBinaryOp::Subtract,
            Self::Multiply => JavaScriptBinaryOp::Multiply,
            Self::Divide => JavaScriptBinaryOp::Divide,
            Self::Remainder => JavaScriptBinaryOp::Remainder,
        }
    }
}

/// The attribute an [`AttributeAssignParts`] writes.
#[derive(Clone, Copy, Debug)]
pub enum AttributeStep<'a> {
    Field(&'a AstString),
    Index(&'a Expr),
}

impl<'a> AttributeAssignParts<'a> {
    /// Reads the parts of an attribute-assignment block, or `None` when the
    /// block does not have the role's shape.
    pub fn of(expr: &'a Expr) -> Option<Self> {
        let Expr::Block(items) = expr else {
            return None;
        };
        let (base, key, result, store, completion) = match items.as_slice() {
            [base, result, store, completion] => (base, None, result, store, completion),
            [base, key, result, store, completion] => (base, Some(key), result, store, completion),
            _ => return None,
        };
        let Expr::Assign {
            target: base_target,
            expr: object,
        } = base
        else {
            return None;
        };
        let Expr::Assign {
            target: result_target,
            expr: value,
        } = result
        else {
            return None;
        };
        let Expr::Assign {
            target: store_target,
            expr: stored,
        } = store
        else {
            return None;
        };
        if !base_target.is_simple()
            || !result_target.is_simple()
            || store_target.root != base_target.root
            || !matches!(stored.as_ref(), Expr::Variable(name) if *name == result_target.root)
            || !matches!(completion, Expr::Variable(name) if *name == result_target.root)
        {
            return None;
        }
        let (step, key_name) = match (store_target.steps.as_slice(), key) {
            ([AssignPathStep::Field(field)], None) => (AttributeStep::Field(field), None),
            (
                [AssignPathStep::Index(Expr::Variable(read))],
                Some(Expr::Assign {
                    target: key_target,
                    expr: index,
                }),
            ) if key_target.is_simple() && *read == key_target.root => {
                (AttributeStep::Index(index), Some(read))
            }
            _ => return None,
        };
        let base = &base_target.root;
        let reads_current = |left: &Expr| match (left, &step, key_name) {
            (Expr::Field { target, field }, AttributeStep::Field(step), None) => {
                matches!(target.as_ref(), Expr::Variable(name) if name == base) && field == *step
            }
            (Expr::Index { target, index }, AttributeStep::Index(_), Some(key)) => {
                matches!(target.as_ref(), Expr::Variable(name) if name == base)
                    && matches!(index.as_ref(), Expr::Variable(name) if name == key)
            }
            _ => false,
        };
        let update = match value.as_ref() {
            Expr::JavaScriptBinary { left, op, right } if reads_current(left) => {
                UpdateOperator::of_javascript(*op).map(|operator| AttributeUpdate {
                    operator,
                    operand: right.as_ref(),
                })
            }
            Expr::Binary { left, op, right } if reads_current(left) => {
                UpdateOperator::of_lashlang(*op).map(|operator| AttributeUpdate {
                    operator,
                    operand: right.as_ref(),
                })
            }
            _ => None,
        };
        // The pinned base (and key) are the role's own slots: the value reads
        // them only as the current attribute an update applies to.
        let rest = update.map_or(value.as_ref(), |update| update.operand);
        if reads_variable(rest, base) || key_name.is_some_and(|key| reads_variable(rest, key)) {
            return None;
        }
        Some(Self {
            object,
            step,
            value,
            value_index: if key.is_some() { 2 } else { 1 },
            update,
        })
    }
}

/// Whether `expr` reads the variable `name` anywhere.
fn reads_variable(expr: &Expr, name: &AstString) -> bool {
    if matches!(expr, Expr::Variable(read) if read == name) {
        return true;
    }
    expr.children().any(|child| reads_variable(child, name))
}

/// The authored parts of a [`StructuralRole::CollectionTransform`] block.
#[derive(Clone, Copy, Debug)]
pub struct CollectionTransformParts<'a> {
    /// The collection the transform reads.
    pub receiver: &'a Expr,
    /// The callback the transform applies.
    pub callback: &'a Expr,
    /// Further operands bound before the driver: an initial value, evaluated
    /// extra arguments, a receiver copy. Empty for the one-callback form.
    pub operands: &'a [Expr],
}

impl<'a> CollectionTransformParts<'a> {
    /// Reads the parts of a collection-transform block, or `None` when the
    /// block does not have the role's shape.
    pub fn of(expr: &'a Expr) -> Option<Self> {
        let Expr::Block(items) = expr else {
            return None;
        };
        let [
            Expr::Assign {
                target: receiver_slot,
                expr: receiver,
            },
            Expr::Assign {
                target: callback_slot,
                expr: callback,
            },
            rest @ ..,
        ] = items.as_slice()
        else {
            return None;
        };
        if !receiver_slot.is_simple()
            || !callback_slot.is_simple()
            || receiver_slot.root == callback_slot.root
        {
            return None;
        }
        let driver = rest.iter().position(|item| {
            matches!(item, Expr::Assign { target, expr }
                if target.is_simple()
                    && matches!(expr.as_ref(), Expr::Function(function)
                        if function.captures.contains(&receiver_slot.root)
                            && function.captures.contains(&callback_slot.root)))
        })?;
        let (operands, after) = rest.split_at(driver);
        let result = &after[1..];
        let simple_assign =
            |item: &Expr| matches!(item, Expr::Assign { target, .. } if target.is_simple());
        if !operands.iter().all(simple_assign)
            || result.is_empty()
            || result.len() > 2
            || result.iter().any(simple_assign)
        {
            return None;
        }
        Some(Self {
            receiver,
            callback,
            operands,
        })
    }
}

/// The [`Expr::children`] path from a [`StructuralRole::ProcessWrapper`]'s
/// wrapped `Try` to its run function's body, and that body.
pub fn process_wrapper_run_path(wrapper: &Expr) -> Option<(Vec<u32>, &Expr)> {
    let Expr::Try(scope) = wrapper else {
        return None;
    };
    let TryExpr {
        body,
        catch: Some(CatchClause {
            binding,
            body: catch,
        }),
        finally: None,
    } = scope.as_ref()
    else {
        return None;
    };
    let Expr::Fail(caught) = catch.as_ref() else {
        return None;
    };
    if !matches!(caught.as_ref(), Expr::Variable(name) if name == binding) {
        return None;
    }
    // Try's children are [body, catch body]; the run call sits in `body`.
    let Expr::Finish(call) = body.as_ref() else {
        return None;
    };
    let Expr::Call { function, .. } = call.as_ref() else {
        return None;
    };
    // Finish -> Call (child 0) -> function (child 0 of the call).
    let mut path = vec![0, 0, 0];
    let run = match function.as_ref() {
        Expr::Function(run) => run,
        Expr::BuiltinCall { args, .. } => match args.first() {
            Some(Expr::Function(run)) => {
                path.push(0);
                run
            }
            _ => return None,
        },
        _ => return None,
    };
    // The function's only child is its body.
    path.push(0);
    Some((path, &run.body))
}

/// Whether a main-level binding belongs to the session or to the front end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingVisibility {
    /// An authored binding: it survives the cell as a session global.
    SessionVisible,
    /// A front end's own slot: it lives only while the cell runs.
    Private,
}

/// Refuses two declarations of one kind that share a name. The linker names
/// the duplicate with its span for an authored program; this is the check an
/// admitted artifact's program is held to.
pub(crate) fn check_unique_declarations(program: &Program) -> Result<(), InvalidAst> {
    let mut names = BTreeSet::new();
    for declaration in &program.declarations {
        let name = match declaration {
            Declaration::Type(declaration) => ("type", declaration.name.as_str()),
            Declaration::Process(declaration) => ("process", declaration.name.as_str()),
            Declaration::Function(declaration) => ("function", declaration.name.as_str()),
        };
        if !names.insert(name) {
            return Err(InvalidAst::DuplicateDeclaration {
                name: name.1.to_string(),
            });
        }
    }
    Ok(())
}

/// A declared process never takes a lifted name; a lifted one carries its
/// literal's digest and has no more hidden parameters than parameters.
pub(super) fn check_process_origins(program: &Program) -> Result<(), InvalidAst> {
    for declaration in &program.declarations {
        let Declaration::Process(process) = declaration else {
            continue;
        };
        let lifted_name = process.name.starts_with(LIFTED_PROCESS_NAME_PREFIX);
        let reason = match &process.origin {
            ProcessOrigin::Declared if lifted_name => {
                "a declared process cannot take a lifted process's name"
            }
            ProcessOrigin::Lifted { .. } if !lifted_name => {
                "a lifted process is named by its literal's digest"
            }
            ProcessOrigin::Lifted { hidden_params, .. }
                if *hidden_params as usize > process.params.len() =>
            {
                "a lifted process has more hidden parameters than parameters"
            }
            _ => continue,
        };
        return Err(InvalidAst::InvalidProcessOrigin {
            process: process.name.to_string(),
            reason,
        });
    }
    Ok(())
}

/// The prefix on every declaration name the linker derives from a lifted
/// process literal. A dialect that authors process names from source text can
/// never collide with one, because the linker invents these and no authored
/// name can start with it by accident.
pub const LIFTED_PROCESS_NAME_PREFIX: &str = "__process_";

/// The name a body at a given AST path lifts to.
///
/// A digest over the canonical body plus the path, so it is a function of what
/// the body *is* and where it sits — never of link order, span tables, or
/// anything else a re-derivation could reorder. The linker's lift and the
/// workflow lens's literal projection must agree on this spelling.
///
/// Domain v2 (FIG-3571): the preimage serializes the carrier IR body, so the
/// same source lifts to a different name than under v1; v1 stays reserved.
pub fn lifted_process_identity(body: &Expr, path: &[u32]) -> String {
    let preimage = serde_json::json!({
        "body": body,
        "path": path,
    });
    let digest = lash_sansio::core_support::blake3_domain_hash_hex(
        "lash-lifted-process-name/v2",
        preimage.to_string(),
    );
    format!("{LIFTED_PROCESS_NAME_PREFIX}{digest}")
}

impl Program {
    /// A main-level binding's visibility role.
    pub fn binding_visibility(&self, name: &str) -> BindingVisibility {
        if self.private_bindings.contains(name) {
            BindingVisibility::Private
        } else {
            BindingVisibility::SessionVisible
        }
    }
}
