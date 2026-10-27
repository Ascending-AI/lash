//! Admission of loop regions from the lowered operations and their library facts.
use super::super::{Action, ActionKind, Atom, Callee, Nested};
use super::*;

/// Why lowering does not admit a formed plan.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// A summary's fixed units are not its program's fixed events.
    Fixed { statement: usize },
    /// A charge region's head states a total its events do not sum to.
    Region { region: u32 },
    /// A program's events are not the stream's, in order.
    Program { statement: usize },
    /// The iteration maximum is not the head's, the dearest path's and the
    /// loop-test unit's sum, or not 0 for an allocating plan.
    Iteration,
    /// A summary's reads or write are not the slots its ops read and
    /// write, or the plan's session bindings are not the ones its ops
    /// use, in order.
    Slots { statement: usize },
    /// The call sites are not the occurrences of the calls its ops make.
    Calls,
    /// The loop-test unit is not the cost table's.
    LoopTest,
    /// The plan's statements, arms or declarations are not the lowered
    /// code's, or its head is not its loop's.
    Shape { statement: usize },
    /// A summary says it allocates where its ops do not, or the other way
    /// round, or the plan does where none of its summaries does.
    Allocates { statement: usize },
}

/// The lowered code a plan is formed and checked against: its stream, its
/// blocks and statements by id, and the libraries it calls.
pub(crate) struct Lowered<'a> {
    pub(crate) stream: &'a Stream,
    pub(crate) block: &'a dyn Fn(BlockId) -> &'a Block,
    pub(crate) stmt: &'a dyn Fn(StmtId) -> &'a Stmt,
    pub(crate) lib: &'a dyn Fn(LibId) -> &'a Lib,
    /// The library function a template's heap path calls.
    pub(crate) lib_of: &'a dyn Fn(&FunctionId) -> Option<LibId>,
    /// Whether declared code has a call island.
    pub(crate) island: &'a dyn Fn(CodeId) -> bool,
}

/// Whether library `function` is an operator a region calls with `count`
/// arguments: it has a prepared entry, and an operation's body makes no
/// cell for a shared parameter at entry, an allocation.
pub(super) fn operator_site(function: &Lib, count: usize) -> bool {
    function.specialization.prepared.is_some()
        && function.arity == count
        && !(function.operation() && function.shares_params)
}

/// Whether library `function` is a native a region calls with `count`
/// arguments through its general call: no operation, whose admission is
/// the reference's, and no plan or body.
pub(super) fn native_site(function: &Lib, count: usize) -> bool {
    matches!(function.run, LibRun::Native { .. })
        && !function.operation()
        && count <= function.arity
}

/// Whether `statement` is a barrier: an op of it is one no region runs,
/// or it is no plain statement or `if`. The region stops before it.
pub(super) fn barrier(lowered: &Lowered<'_>, statement: &Stmt) -> bool {
    if !matches!(statement.form, Form::Plain | Form::If { .. }) {
        return true;
    }
    let stream = lowered.stream;
    let Some(ops) = stream
        .ops
        .get(statement.ops.start as usize..statement.ops.end() as usize)
    else {
        return true;
    };
    if !writes_last(lowered, ops) {
        return true;
    }
    ops.iter().any(|op| match *op {
        Op::Region { .. }
        | Op::Local { .. }
        | Op::Session { .. }
        | Op::WriteSession { .. }
        | Op::MakeTuple { .. }
        | Op::MakeList { .. }
        | Op::MakeSet { .. }
        | Op::MakeMap { .. }
        | Op::MakeRecord { .. }
        | Op::Field { .. }
        | Op::Index { .. }
        | Op::Pin { .. }
        | Op::Test { .. }
        | Op::Branch { .. }
        | Op::WriteLocal { .. }
        | Op::Bind { .. }
        | Op::Continue
        | Op::SetField { .. }
        | Op::SetIndex { .. }
        | Op::LetBind { .. }
        | Op::LetEnd { .. }
        | Op::JumpIfFalse { .. }
        | Op::Jump { .. }
        | Op::End => false,
        Op::Const { c, .. } => !matches!(stream.consts.get(c.ix()), Some(Constant::Resident(_))),
        Op::Operator { lib, args } => {
            !operator_site((lowered.lib)(LibId(lib.0)), args.len as usize)
        }
        Op::Nested { act, args } => match stream.nested.get(act.ix()) {
            Some(nested) if nested.operator => match nested.kind.operator_lib() {
                Some(lib) => !operator_site((lowered.lib)(lib), args.len as usize),
                None => true,
            },
            Some(nested) => !guest_site(nested),
            None => true,
        },
        Op::Call { lib, args } => !native_site((lowered.lib)(LibId(lib.0)), args.len as usize),
        _ => true,
    })
}

/// Whether operator `lib`'s heap paths, or a callee's entry below them,
/// may write a guest object: an installed native answers where its entry
/// declines, so its heap paths never run.
pub(super) fn writes(lowered: &Lowered<'_>, lib: LibId) -> bool {
    let function = (lowered.lib)(lib);
    match &function.specialization.prepared {
        Some((entry, _)) if !matches!(function.run, LibRun::Native { .. }) => {
            entry.writes(lowered.lib, lowered.lib_of)
        }
        _ => false,
    }
}

/// Whether the statement of `ops` calls an operator that writes
/// ([`writes`]) only as its last evaluation before the `do` that ends it,
/// the call whose value the `do` discards: then what it writes commits
/// with the statement, and nothing the statement evaluates after reads
/// the object the write is held for.
pub(super) fn writes_last(lowered: &Lowered<'_>, ops: &[Op]) -> bool {
    let stream = lowered.stream;
    let writer = |op: &Op| {
        let lib = match *op {
            Op::Operator { lib, .. } => Some(LibId(lib.0)),
            Op::Nested { act, .. } => stream
                .nested
                .get(act.ix())
                .and_then(|nested| nested.kind.operator_lib()),
            _ => None,
        };
        lib.is_some_and(|lib| writes(lowered, lib))
    };
    let before = match ops {
        [before @ .., _, Op::End] => before,
        _ => ops,
    };
    !before.iter().any(writer)
}

/// Whether nested action `nested` is a guest call that may enter its
/// callee's island: a closure's, or a declared function's that has one.
pub(super) fn guest_site(nested: &Nested) -> bool {
    nested.island
        && matches!(
            nested.kind,
            NestedKind::Call(NestedCallee::Read | NestedCallee::Declared(_))
        )
}

/// Whether `statement` makes a guest call a region may run through its
/// callee's island: one its derivation may refuse, which leaves the
/// statement to the reference as a barrier is.
pub(super) fn guest_call(lowered: &Lowered<'_>, statement: &Stmt) -> bool {
    if let Form::Action(action) = &statement.form {
        return matches!(
            action.kind,
            ActionKind::Call {
                callee: Callee::Declared(_) | Callee::Value(_),
                ..
            }
        );
    }
    let stream = lowered.stream;
    stream
        .ops
        .get(statement.ops.start as usize..statement.ops.end() as usize)
        .is_some_and(|ops| {
            ops.iter().any(|op| {
                matches!(op, Op::Nested { act, .. }
                    if stream.nested.get(act.ix()).is_some_and(guest_site))
            })
        })
}

/// Whether `op` allocates, calls a native, reads a member, holds a value
/// other than a scalar or declares an expression's variable, whose memory
/// it holds: what makes its statement's plan allocate.
pub(super) fn general<'a>(stream: &Stream, lib: &dyn Fn(LibId) -> &'a Lib, op: Op) -> bool {
    match op {
        Op::Call { .. }
        | Op::MakeTuple { .. }
        | Op::MakeList { .. }
        | Op::MakeSet { .. }
        | Op::MakeMap { .. }
        | Op::MakeRecord { .. }
        | Op::Field { .. }
        | Op::Index { .. }
        | Op::Pin { .. }
        | Op::SetField { .. }
        | Op::SetIndex { .. }
        | Op::LetBind { .. }
        | Op::End => true,
        Op::Const { c, .. } => !matches!(
            stream.consts.get(c.ix()),
            Some(Constant::Resident(value)) if scalar(value)
        ),
        Op::Operator { lib: id, args } => !scalar_site(lib(LibId(id.0)), args.len as usize),
        Op::Nested { act, args } => match stream
            .nested
            .get(act.ix())
            .and_then(|nested| nested.kind.operator_lib())
        {
            Some(id) => !scalar_site(lib(id), args.len as usize),
            None => true,
        },
        _ => false,
    }
}

/// Whether operator `function` with `count` arguments has the scalar table
/// a scalar plan charges it, over one or two operands, and an entry that
/// answers immediates: a template with heap paths alone is an allocating
/// plan's. A scalar plan's operator takes no heap path.
pub(super) fn scalar_site(function: &Lib, count: usize) -> bool {
    (1..=2).contains(&count)
        && function
            .specialization
            .prepared
            .as_ref()
            .is_some_and(|(entry, fast)| fast.scalar().is_some() && entry.immediate())
}

/// What forming a plan gathers across its statements: its session
/// bindings in the order they are first used.
#[derive(Default)]
pub(super) struct Gathered {
    pub(super) sessions: Vec<BindingName>,
}

impl Gathered {
    /// The session slot of `name`, given one on its first use.
    pub(super) fn session(&mut self, name: &BindingName) -> RegionSlot {
        let at = match self.sessions.iter().position(|held| held.name == name.name) {
            Some(at) => at,
            None => {
                self.sessions.push(name.clone());
                self.sessions.len() - 1
            }
        };
        RegionSlot::Session(at as u32)
    }
}

/// The call a statement's root call action makes: its callee and its
/// arguments.
pub(super) fn root_call(form: &Form) -> Option<(&Action, &Callee, &[Atom])> {
    let Form::Action(action) = form else {
        return None;
    };
    let ActionKind::Call { callee, args } = &action.kind else {
        return None;
    };
    Some((action, callee, args))
}

/// The statically named library invoke of a statement action.
pub(super) fn library_invoke(form: &Form) -> Option<(&Action, LibId, &[Atom])> {
    let Form::Action(action) = form else {
        return None;
    };
    let ActionKind::Call { callee, args } = &action.kind else {
        return None;
    };
    Some((action, callee.library()?, args))
}
