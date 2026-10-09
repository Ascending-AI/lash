//! Lexical resolution over the IR a draft spells.
//!
//! A variable is a name in a frame. `main`, each process body, each declared
//! function, each function value and each process literal is a frame of its
//! own with one slot per name; a frame reads an enclosing one only through a
//! function's captures or a literal's hidden arguments, which copy the value
//! under the same name. A name is visible at a read when a binder for it
//! precedes the read in evaluation order: the two branches of an `if` and the
//! body and catch of a `try` each start from what was visible before them,
//! and a loop element or a catch binding is visible only inside its region.
//! A process reference and a function call name a declaration of the
//! program, from any frame. These are the linker's rules, read without a host
//! environment.

use std::collections::BTreeSet;

use crate::ast::{
    AssignPathStep, AstPath, AstRoot, AstString, Declaration, Expr, ProcessWrapperParts, Program,
    StructuralRole, process_wrapper_run_path,
};

/// The names every process body may read without binding them.
const PROCESS_INPUTS: [&str; 2] = ["input", "inputs"];

/// The frames of a program and every name occurrence in them.
pub(super) struct Lexical {
    pub(super) frames: Vec<Frame>,
    pub(super) occurrences: Vec<Occurrence>,
    /// Names a read resolves to with no binder in its frame: declared
    /// processes, and `main` bindings of a process literal, which a literal's
    /// body reads as the process the binding lifts to.
    pub(super) processes: BTreeSet<AstString>,
    /// The declared functions, which a call names from any frame.
    pub(super) functions: BTreeSet<AstString>,
}

pub(super) struct Frame {
    pub(super) root: FrameRoot,
    pub(super) parent: Option<usize>,
    /// The names the frame's own header binds: parameters, a function's name
    /// and receiver, captures and hidden arguments.
    pub(super) params: BTreeSet<AstString>,
}

/// Where a frame's body hangs in the program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum FrameRoot {
    Main,
    Declaration(u32),
    /// The function value or process literal at this path.
    Expr(AstPath),
}

pub(super) struct Occurrence {
    pub(super) frame: usize,
    pub(super) name: AstString,
    /// The expression that holds the occurrence.
    pub(super) path: AstPath,
    pub(super) role: Role,
    /// Whether a binder for the name is visible here.
    pub(super) bound: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    Read,
    /// A plain assignment, a loop element or a catch binding.
    Bind,
    /// An assignment through a path, which needs its root bound.
    Update,
    /// A capture or a hidden argument: a read in the enclosing frame.
    Capture,
    /// A reference to a declared process.
    Process,
    /// A call to a declared function.
    Function,
}

impl Lexical {
    pub(super) fn of(program: &Program) -> Self {
        let mut lexical = Self {
            frames: Vec::new(),
            occurrences: Vec::new(),
            processes: BTreeSet::new(),
            functions: BTreeSet::new(),
        };
        for declaration in &program.declarations {
            match declaration {
                Declaration::Process(process) => {
                    lexical.processes.insert(process.name.clone());
                }
                Declaration::Function(function) => {
                    lexical.functions.insert(function.name.clone());
                }
            }
        }
        collect_literal_bindings(&program.main, &mut lexical.processes);
        let main = lexical.frame(FrameRoot::Main, None, BTreeSet::new());
        lexical.walk(
            &program.main,
            &AstPath::main(Vec::new()),
            main,
            &mut BTreeSet::new(),
        );
        for (index, declaration) in (0u32..).zip(&program.declarations) {
            let (params, body, process): (Vec<AstString>, &Expr, bool) = match declaration {
                Declaration::Process(process) => (
                    process
                        .params
                        .iter()
                        .map(|param| param.name.clone())
                        .collect(),
                    &process.body,
                    true,
                ),
                Declaration::Function(function) => (
                    function
                        .params
                        .iter()
                        .map(|param| param.name.clone())
                        .collect(),
                    &function.body,
                    false,
                ),
            };
            let frame = lexical.frame(FrameRoot::Declaration(index), None, params.iter().cloned());
            let mut scope = params.into_iter().collect::<BTreeSet<_>>();
            if process {
                scope.extend(PROCESS_INPUTS.map(AstString::from));
            }
            lexical.walk(
                body,
                &AstPath::declaration(index, Vec::new()),
                frame,
                &mut scope,
            );
        }
        lexical
    }

    /// The reads no binder reaches.
    pub(super) fn unresolved(&self) -> impl Iterator<Item = &Occurrence> {
        self.occurrences.iter().filter(|occurrence| {
            !occurrence.bound
                && match occurrence.role {
                    Role::Read | Role::Update | Role::Capture => {
                        !self.processes.contains(&occurrence.name)
                    }
                    Role::Process | Role::Function => true,
                    Role::Bind => false,
                }
        })
    }

    /// The outermost frame `frame` hangs from: `main` or a declaration.
    pub(super) fn top(&self, mut frame: usize) -> &FrameRoot {
        while let Some(parent) = self.frames[frame].parent {
            frame = parent;
        }
        &self.frames[frame].root
    }

    /// The innermost frame whose body holds the expression at `path`.
    pub(super) fn frame_at(&self, path: &AstPath) -> Option<usize> {
        let mut best: Option<(usize, usize)> = None;
        for (index, frame) in self.frames.iter().enumerate() {
            let depth = match &frame.root {
                FrameRoot::Main if path.root == AstRoot::Main => 0,
                FrameRoot::Declaration(declaration)
                    if path.root == AstRoot::Declaration(*declaration) =>
                {
                    0
                }
                FrameRoot::Expr(root)
                    if root.root == path.root
                        && path.steps.len() > root.steps.len()
                        && path.steps.starts_with(&root.steps) =>
                {
                    root.steps.len() + 1
                }
                _ => continue,
            };
            if best.is_none_or(|(_, deepest)| depth >= deepest) {
                best = Some((index, depth));
            }
        }
        best.map(|(index, _)| index)
    }

    /// Whether `frame` is `ancestor` or hangs from it.
    pub(super) fn within(&self, mut frame: usize, ancestor: usize) -> bool {
        loop {
            if frame == ancestor {
                return true;
            }
            match self.frames[frame].parent {
                Some(parent) => frame = parent,
                None => return false,
            }
        }
    }

    fn frame(
        &mut self,
        root: FrameRoot,
        parent: Option<usize>,
        params: impl IntoIterator<Item = AstString>,
    ) -> usize {
        self.frames.push(Frame {
            root,
            parent,
            params: params.into_iter().collect(),
        });
        self.frames.len() - 1
    }

    fn occurrence(
        &mut self,
        frame: usize,
        name: &AstString,
        path: &AstPath,
        role: Role,
        bound: bool,
    ) {
        self.occurrences.push(Occurrence {
            frame,
            name: name.clone(),
            path: path.clone(),
            role,
            bound,
        });
    }

    fn walk(&mut self, expr: &Expr, path: &AstPath, frame: usize, scope: &mut BTreeSet<AstString>) {
        match expr {
            Expr::Variable(name) => {
                self.occurrence(frame, name, path, Role::Read, scope.contains(name));
            }
            Expr::ProcessRef { process } => {
                let known = self.processes.contains(process);
                self.occurrence(frame, process, path, Role::Process, known);
            }
            Expr::FunctionCall { function, args } => {
                let declared = self.functions.contains(function);
                self.occurrence(frame, function, path, Role::Function, declared);
                for (index, argument) in (0u32..).zip(args) {
                    self.walk(argument, &path.child(index), frame, scope);
                }
            }
            Expr::Assign { target, expr } => {
                let mut child = 0;
                for step in &target.steps {
                    if let AssignPathStep::Index(index) = step {
                        self.walk(index, &path.child(child), frame, scope);
                        child += 1;
                    }
                }
                self.walk(expr, &path.child(child), frame, scope);
                if target.is_simple() {
                    self.occurrence(frame, &target.root, path, Role::Bind, true);
                    scope.insert(target.root.clone());
                } else {
                    let bound = scope.contains(&target.root);
                    self.occurrence(frame, &target.root, path, Role::Update, bound);
                }
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                self.walk(condition, &path.child(0), frame, scope);
                let mut then_scope = scope.clone();
                self.walk(then_block, &path.child(1), frame, &mut then_scope);
                let mut else_scope = scope.clone();
                self.walk(else_block, &path.child(2), frame, &mut else_scope);
                scope.extend(then_scope);
                scope.extend(else_scope);
            }
            Expr::For {
                binding,
                iterable,
                bind,
                body,
                ..
            } => {
                self.walk(iterable, &path.child(0), frame, scope);
                let mut body_scope = scope.clone();
                let outer = body_scope.contains(binding);
                self.occurrence(frame, binding, path, Role::Bind, true);
                body_scope.insert(binding.clone());
                if let Some(bind) = bind {
                    self.walk(bind, &path.child(1), frame, &mut body_scope);
                }
                let body_index = Expr::for_body_index(bind.as_deref());
                self.walk(body, &path.child(body_index), frame, &mut body_scope);
                if !outer {
                    body_scope.remove(binding);
                }
                scope.extend(body_scope);
            }
            Expr::While { condition, body } => {
                self.walk(condition, &path.child(0), frame, scope);
                let mut body_scope = scope.clone();
                self.walk(body, &path.child(1), frame, &mut body_scope);
                scope.extend(body_scope);
            }
            Expr::Try(region) => {
                let mut try_scope = scope.clone();
                self.walk(&region.body, &path.child(0), frame, &mut try_scope);
                let mut next = 1;
                if let Some(catch) = &region.catch {
                    let mut catch_scope = scope.clone();
                    let outer = catch_scope.contains(&catch.binding);
                    self.occurrence(frame, &catch.binding, path, Role::Bind, true);
                    catch_scope.insert(catch.binding.clone());
                    self.walk(&catch.body, &path.child(next), frame, &mut catch_scope);
                    if !outer {
                        catch_scope.remove(&catch.binding);
                    }
                    try_scope.extend(catch_scope);
                    next += 1;
                }
                *scope = try_scope;
                if let Some(finally) = &region.finally {
                    self.walk(finally, &path.child(next), frame, scope);
                }
            }
            Expr::Function(function) => {
                for capture in &function.captures {
                    let bound = scope.contains(capture);
                    self.occurrence(frame, capture, path, Role::Capture, bound);
                }
                let params = function
                    .captures
                    .iter()
                    .chain(&function.params)
                    .chain(&function.name)
                    .chain(&function.receiver)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let inner = self.frame(FrameRoot::Expr(path.clone()), Some(frame), params.clone());
                let mut scope = params;
                self.walk(&function.body, &path.child(0), inner, &mut scope);
            }
            Expr::ProcessLiteral(literal) => {
                for hidden in &literal.hidden_args {
                    let bound = scope.contains(&hidden.name);
                    self.occurrence(frame, &hidden.name, path, Role::Capture, bound);
                }
                let params = literal
                    .params
                    .iter()
                    .chain(&literal.hidden_args)
                    .map(|param| param.name.clone())
                    .collect::<BTreeSet<_>>();
                let inner = self.frame(FrameRoot::Expr(path.clone()), Some(frame), params.clone());
                let mut scope = params;
                scope.extend(PROCESS_INPUTS.map(AstString::from));
                self.walk(&literal.body, &path.child(0), inner, &mut scope);
            }
            _ => {
                for (index, child) in (0u32..).zip(expr.children()) {
                    self.walk(child, &path.child(index), frame, scope);
                }
            }
        }
    }
}

/// The `main` names bound straight to a process literal.
fn collect_literal_bindings(expr: &Expr, names: &mut BTreeSet<AstString>) {
    if let Expr::Assign { target, expr } = expr
        && target.is_simple()
        && matches!(expr.as_ref(), Expr::ProcessLiteral(_))
    {
        names.insert(target.root.clone());
    }
    for child in expr.children() {
        collect_literal_bindings(child, names);
    }
}

/// Whether a process wrapper passes the process's `name` straight through to
/// its run function's parameter of the same name, so the two frames spell one
/// authored parameter.
fn passes_through(wrapper: &Expr, name: &AstString) -> bool {
    ProcessWrapperParts::of(wrapper).is_some_and(|parts| {
        parts
            .run
            .params
            .iter()
            .zip(parts.arguments)
            .any(|(param, argument)| {
                param == name && matches!(argument, Expr::Variable(passed) if passed == name)
            })
    })
}

/// The run function of a process wrapper's `Try`.
fn wrapper_run_mut(wrapper: &mut Expr) -> Option<&mut crate::FunctionExpr> {
    let Expr::Try(region) = wrapper else {
        return None;
    };
    let Expr::Finish(call) = region.body.as_mut() else {
        return None;
    };
    let Expr::Call { function, .. } = call.as_mut() else {
        return None;
    };
    match function.as_mut() {
        Expr::Function(run) => Some(run),
        Expr::BuiltinCall { args, .. } => match args.first_mut() {
            Some(Expr::Function(run)) => Some(run),
            _ => None,
        },
        _ => None,
    }
}

/// The frame that owns the variable a body in `frame` reads as `name`: a
/// capture, a hidden argument and a passed-through process parameter all
/// spell the enclosing frame's variable.
pub(super) fn owning_frame(
    lexical: &Lexical,
    program: &Program,
    mut frame: usize,
    name: &AstString,
) -> usize {
    while let (FrameRoot::Expr(path), Some(parent)) =
        (&lexical.frames[frame].root, lexical.frames[frame].parent)
    {
        let copied = match expr_at(program, path) {
            Some(Expr::Function(function)) => {
                function.captures.contains(name)
                    || is_passed_through_run(lexical, program, parent, path, name)
            }
            Some(Expr::ProcessLiteral(literal)) => {
                literal
                    .hidden_args
                    .iter()
                    .any(|hidden| hidden.name == *name)
                    && !literal.params.iter().any(|param| param.name == *name)
            }
            _ => false,
        };
        if !copied {
            break;
        }
        frame = parent;
    }
    frame
}

/// Whether the function at `path` is the run function of `parent`'s process
/// wrapper and receives `name` passed straight through.
fn is_passed_through_run(
    lexical: &Lexical,
    program: &Program,
    parent: usize,
    path: &AstPath,
    name: &AstString,
) -> bool {
    let body = match &lexical.frames[parent].root {
        FrameRoot::Main => return false,
        FrameRoot::Declaration(index) => AstPath::declaration(*index, Vec::new()),
        FrameRoot::Expr(literal) => literal.child(0),
    };
    let Some(Expr::Role {
        role: StructuralRole::ProcessWrapper,
        expr: wrapper,
    }) = expr_at(program, &body)
    else {
        return false;
    };
    let Some((steps, _)) = process_wrapper_run_path(wrapper) else {
        return false;
    };
    let mut run = body.child(0);
    run.steps.extend(&steps[..steps.len().saturating_sub(1)]);
    run == *path && passes_through(wrapper, name)
}

/// Renames the variable `from` of the frame at `root` to `to`, in every
/// binder and use, following it into the frames that copy it.
pub(super) fn rename(program: &mut Program, root: &FrameRoot, from: &AstString, to: &AstString) {
    match root {
        FrameRoot::Main => {
            rename_in(&mut program.main, from, to);
            if program.private_bindings.remove(from) {
                program.private_bindings.insert(to.clone());
            }
        }
        FrameRoot::Declaration(index) => match program.declarations.get_mut(*index as usize) {
            Some(Declaration::Process(process)) => {
                for param in &mut process.params {
                    rename_name(&mut param.name, from, to);
                }
                rename_in(&mut process.body, from, to);
            }
            Some(Declaration::Function(function)) => {
                for param in &mut function.params {
                    rename_name(&mut param.name, from, to);
                }
                rename_in(&mut function.body, from, to);
            }
            None => {}
        },
        FrameRoot::Expr(path) => match expr_at_mut(program, path) {
            Some(Expr::Function(function)) => {
                for name in function
                    .params
                    .iter_mut()
                    .chain(&mut function.name)
                    .chain(&mut function.receiver)
                {
                    rename_name(name, from, to);
                }
                rename_in(&mut function.body, from, to);
            }
            Some(Expr::ProcessLiteral(literal)) => {
                for param in &mut literal.params {
                    rename_name(&mut param.name, from, to);
                }
                rename_in(&mut literal.body, from, to);
            }
            _ => {}
        },
    }
}

fn rename_name(name: &mut AstString, from: &AstString, to: &AstString) {
    if name == from {
        *name = to.clone();
    }
}

/// Renames `from` at the level of one frame's body.
fn rename_in(expr: &mut Expr, from: &AstString, to: &AstString) {
    match expr {
        Expr::Variable(name) => rename_name(name, from, to),
        Expr::Function(function) => {
            let own = function
                .params
                .iter()
                .chain(&function.name)
                .chain(&function.receiver)
                .any(|name| name == from);
            let mut captured = false;
            for capture in &mut function.captures {
                captured |= capture == from;
                rename_name(capture, from, to);
            }
            if captured && !own {
                rename_in(&mut function.body, from, to);
            }
        }
        Expr::ProcessLiteral(literal) => {
            let own = literal.params.iter().any(|param| param.name == *from);
            let mut hidden = false;
            for argument in &mut literal.hidden_args {
                hidden |= argument.name == *from;
                rename_name(&mut argument.name, from, to);
            }
            if hidden && !own {
                rename_in(&mut literal.body, from, to);
            }
        }
        Expr::Role {
            role: StructuralRole::ProcessWrapper,
            expr: wrapper,
        } if passes_through(wrapper, from) => {
            for child in wrapper.children_mut() {
                rename_in(child, from, to);
            }
            if let Some(run) = wrapper_run_mut(wrapper) {
                for param in &mut run.params {
                    rename_name(param, from, to);
                }
                rename_in(&mut run.body, from, to);
            }
        }
        _ => {
            match expr {
                Expr::Assign { target, .. } => rename_name(&mut target.root, from, to),
                Expr::For { binding, .. } => rename_name(binding, from, to),
                Expr::Try(region) => {
                    if let Some(catch) = &mut region.catch {
                        rename_name(&mut catch.binding, from, to);
                    }
                }
                _ => {}
            }
            for child in expr.children_mut() {
                rename_in(child, from, to);
            }
        }
    }
}

/// Renames the declared process `from` to `to`, in its declaration and in
/// every reference to it.
pub(super) fn rename_process(program: &mut Program, from: &AstString, to: &AstString) {
    fn references(expr: &mut Expr, from: &AstString, to: &AstString) {
        if let Expr::ProcessRef { process } = expr {
            rename_name(process, from, to);
        }
        for child in expr.children_mut() {
            references(child, from, to);
        }
    }
    references(&mut program.main, from, to);
    for declaration in &mut program.declarations {
        match declaration {
            Declaration::Process(process) => {
                rename_name(&mut process.name, from, to);
                references(&mut process.body, from, to);
            }
            Declaration::Function(function) => references(&mut function.body, from, to),
        }
    }
}

use super::super::projection::expr_at;

/// The mutable twin of [`expr_at`].
fn expr_at_mut<'p>(program: &'p mut Program, path: &AstPath) -> Option<&'p mut Expr> {
    let mut expression = match path.root {
        AstRoot::Main => &mut program.main,
        AstRoot::Declaration(index) => match program.declarations.get_mut(index as usize)? {
            Declaration::Process(process) => &mut process.body,
            Declaration::Function(function) => &mut function.body,
        },
    };
    for step in &path.steps {
        expression = expression.children_mut().nth(*step as usize)?;
    }
    Some(expression)
}
