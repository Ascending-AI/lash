//! Statement and source-note ownership for dialect lowering.
use std::collections::BTreeMap;

use crate::Note;
use lash_kernel_doc::{
    Block, CallablePresentation, Closure, Expr, FunctionDefinition, Node, Place, Rhs, Stmt,
};
use lash_kernel_value::{FunctionId, Name};

/// A kernel block under construction: each statement with the note of
/// its source layers, so a move, removal or fold carries both.
#[derive(Clone, Debug, Default)]
pub struct BlockBuilder {
    pub implicit_return: bool,
    emitted: Vec<Emitted>,
}

#[derive(Clone, Debug)]
struct Emitted {
    stmt: Stmt,
    note: Note,
}

impl BlockBuilder {
    /// The block of the one statement `stmt`.
    pub fn one(stmt: Stmt, note: Note) -> Self {
        Self {
            implicit_return: false,
            emitted: vec![Emitted { stmt, note }],
        }
    }

    pub fn len(&self) -> usize {
        self.emitted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.emitted.is_empty()
    }

    pub fn stmts(&self) -> impl DoubleEndedIterator<Item = &Stmt> + ExactSizeIterator {
        self.emitted.iter().map(|emitted| &emitted.stmt)
    }

    pub fn notes(&self) -> impl Iterator<Item = &Note> {
        self.emitted.iter().map(|emitted| &emitted.note)
    }

    pub fn last_stmt(&self) -> Option<&Stmt> {
        self.emitted.last().map(|emitted| &emitted.stmt)
    }

    pub fn last_note_mut(&mut self) -> Option<&mut Note> {
        self.emitted.last_mut().map(|emitted| &mut emitted.note)
    }

    /// The statement at `index` and its note, to change.
    pub fn get_mut(&mut self, index: usize) -> Option<(&Stmt, &mut Note)> {
        self.emitted
            .get_mut(index)
            .map(|emitted| (&emitted.stmt, &mut emitted.note))
    }

    pub fn push(&mut self, stmt: Stmt, note: Note) {
        self.emitted.push(Emitted { stmt, note });
    }

    pub fn insert(&mut self, index: usize, stmt: Stmt, note: Note) {
        self.emitted.insert(index, Emitted { stmt, note });
    }

    pub fn pop(&mut self) -> Option<(Stmt, Note)> {
        self.emitted
            .pop()
            .map(|emitted| (emitted.stmt, emitted.note))
    }

    /// Puts `buf`'s statements before the one at `index`.
    pub fn splice(&mut self, index: usize, buf: Self) {
        self.emitted.splice(index..index, buf.emitted);
    }

    pub fn extend(&mut self, buf: Self) {
        if !buf.is_empty() {
            self.implicit_return = buf.implicit_return;
        }
        self.emitted.extend(buf.emitted);
    }

    /// The finished block, and the notes of its statements in order.
    pub fn into_parts(self) -> (Block, Vec<Note>) {
        self.emitted
            .into_iter()
            .map(|emitted| (emitted.stmt, emitted.note))
            .unzip()
    }

    pub fn into_notes(self) -> impl Iterator<Item = Note> {
        self.emitted.into_iter().map(|emitted| emitted.note)
    }
}

impl BlockBuilder {
    pub fn from_parts(statements: Block, notes: Vec<Note>) -> Self {
        assert_eq!(
            statements.len(),
            notes.len(),
            "each statement owns one note"
        );
        Self {
            implicit_return: false,
            emitted: statements
                .into_iter()
                .zip(notes)
                .map(|(stmt, note)| Emitted { stmt, note })
                .collect(),
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&Stmt) -> bool) {
        self.emitted.retain(|emitted| keep(&emitted.stmt));
    }

    /// Fold only bindings, keeping actions lazy and preserving K-DOC-006.
    pub fn expression(&self, mut body: Expr, kernel_depth: usize) -> Option<Expr> {
        for stmt in self.stmts().rev() {
            let Stmt::Let { name, value } = stmt else {
                return None;
            };
            let value = match value {
                Rhs::Expr(value) => value.clone(),
                Rhs::Action(action) => action.clone().into(),
            };
            body = Expr::Let {
                name: name.clone(),
                value: Box::new(value),
                body: Box::new(body),
            };
        }
        let mut pending = vec![(Node::Expr(&body), 0)];
        while let Some((node, depth)) = pending.pop() {
            if depth + 2 * kernel_depth + 4 > lash_kernel_doc::MAX_NESTING_DEPTH {
                return None;
            }
            pending.extend(node.children().into_iter().map(|child| (child, depth + 1)));
        }
        Some(body)
    }

    pub fn written(&self, prefix: &[u32]) -> Vec<(Vec<u32>, crate::Written)> {
        let mut out = Vec::new();
        let mut prefix = prefix.to_vec();
        for note in self.notes() {
            if let Some(written) = &note.written {
                let mut path = prefix.clone();
                path.push(0);
                out.push((path, written.clone()));
            }
            for (path, written) in &note.nested_written {
                let mut nested = prefix.clone();
                nested.extend(path);
                out.push((nested, written.clone()));
            }
            prefix.push(1);
        }
        out
    }

    /// Finish a lazy value and move its closure notes with the folded bindings.
    pub fn folded(
        self,
        value: Expr,
        kernel_depth: usize,
        prefix: &[u32],
    ) -> Result<(Expr, Note), Self> {
        let Some(value) = self.expression(value, kernel_depth) else {
            return Err(self);
        };
        let nested_written = self.written(prefix);
        let blocks = self.into_notes().flat_map(|note| note.blocks).collect();
        Ok((
            value,
            Note {
                blocks,
                nested_written,
                ..Note::default()
            },
        ))
    }

    pub fn conditional(condition: Expr, then: Self, otherwise: Self) -> Self {
        let (then_block, then_note) = then.into_parts();
        let (else_block, else_note) = otherwise.into_parts();
        Self::one(
            Stmt::If {
                condition,
                then_block,
                else_block,
            },
            Note {
                blocks: vec![then_note, else_note],
                ..Note::default()
            },
        )
    }
}

/// Capture a region without making its bindings visible in the enclosing buffer.
pub fn region<S, T, E>(
    state: &mut S,
    buffer: impl Fn(&mut S) -> &mut BlockBuilder,
    lower: impl FnOnce(&mut S) -> Result<T, E>,
) -> Result<(BlockBuilder, T), E> {
    let outer = std::mem::take(buffer(state));
    let result = lower(state);
    let inner = std::mem::replace(buffer(state), outer);
    result.map(|value| (inner, value))
}

pub struct Choice<T> {
    pub note: Note,
    pub branches: Branches<T>,
}

pub enum Branches<T> {
    Expression {
        value: Expr,
    },
    Statements {
        condition: Expr,
        then_branch: (BlockBuilder, T),
        else_branch: (BlockBuilder, T),
    },
}

impl<T> Choice<T> {
    /// Finish a choice through the dialect's operand, store and emission rules.
    pub fn finish<S, U>(
        self,
        state: &mut S,
        facts: U,
        expression: impl FnOnce(&mut S, Expr, Note, U) -> T,
        result: impl FnOnce(&mut S, U) -> (Place, T),
        mut store: impl FnMut(&mut S, (BlockBuilder, T), Place) -> BlockBuilder,
        emit: impl FnOnce(&mut S, Expr, BlockBuilder, BlockBuilder),
    ) -> T {
        match self.branches {
            Branches::Expression { value } => expression(state, value, self.note, facts),
            Branches::Statements {
                condition,
                then_branch,
                else_branch,
            } => {
                let (place, value) = result(state, facts);
                let then = store(state, then_branch, place.clone());
                let otherwise = store(state, else_branch, place);
                emit(state, condition, then, otherwise);
                value
            }
        }
    }
}

/// Keep folded closure and written notes on a binding when an inline value
/// has no statement to own them.
pub fn finish_expression<S, T, U>(
    state: &mut S,
    value: Expr,
    note: Note,
    facts: U,
    inline: impl FnOnce(&mut S, Expr, U) -> T,
    bind: impl FnOnce(&mut S, Expr, U) -> T,
    buffer: impl Fn(&mut S) -> &mut BlockBuilder,
) -> T {
    if note.blocks.is_empty() && note.nested_written.is_empty() {
        return inline(state, value, facts);
    }
    let result = bind(state, value, facts);
    if let Some(placed) = buffer(state).last_note_mut() {
        placed.blocks = note.blocks;
        placed.nested_written = note.nested_written;
    }
    result
}

/// Commit operand regions in source order. An unstable operand is pinned
/// before any later region's statements can change its value (K-EVAL-001).
pub fn ordered<S, T>(
    state: &mut S,
    parts: Vec<(BlockBuilder, T)>,
    stable: impl Fn(&T) -> bool,
    mut append: impl FnMut(&mut S, BlockBuilder),
    mut pin: impl FnMut(&mut S, T) -> T,
) -> Vec<T> {
    let mut pins = vec![false; parts.len()];
    let mut later = false;
    for (index, (buf, operand)) in parts.iter().enumerate().rev() {
        pins[index] = later && !stable(operand);
        later |= !buf.is_empty();
    }
    parts
        .into_iter()
        .zip(pins)
        .map(|((buf, operand), pinned)| {
            append(state, buf);
            if pinned { pin(state, operand) } else { operand }
        })
        .collect()
}

/// Store a finished local function and its source notes before binding its closure.
pub fn closure_binding(
    name: Name,
    definition: FunctionDefinition,
    notes: Vec<Note>,
    presentation: Option<CallablePresentation>,
    functions: &mut BTreeMap<FunctionId, FunctionDefinition>,
    local_notes: &mut BTreeMap<FunctionId, Vec<Note>>,
) -> Stmt {
    let function = definition.identity();
    functions.insert(function, definition);
    local_notes.insert(function, notes);
    Stmt::Let {
        name,
        value: Rhs::Expr(Expr::Closure(Box::new(Closure {
            function,
            presentation,
        }))),
    }
}

/// Join lazy regions. Source type joins and statement-only stores remain with the dialect.
pub fn choose<T>(
    condition: Expr,
    then_branch: (BlockBuilder, T),
    else_branch: (BlockBuilder, T),
    kernel_depth: usize,
    expression: impl Fn(&T) -> Expr,
) -> Choice<T> {
    let (then, left) = then_branch;
    let (otherwise, right) = else_branch;
    if let (Some(left), Some(right)) = (
        then.expression(expression(&left), kernel_depth),
        otherwise.expression(expression(&right), kernel_depth),
    ) {
        let nested_written = then
            .written(&[0, 1])
            .into_iter()
            .chain(otherwise.written(&[0, 2]))
            .collect();
        let blocks = then
            .into_notes()
            .chain(otherwise.into_notes())
            .flat_map(|note| note.blocks)
            .collect();
        Choice {
            branches: Branches::Expression {
                value: Expr::If {
                    cond: Box::new(condition),
                    then: Box::new(left),
                    r#else: Box::new(right),
                },
            },
            note: Note {
                blocks,
                nested_written,
                ..Note::default()
            },
        }
    } else {
        Choice {
            note: Note::default(),
            branches: Branches::Statements {
                condition,
                then_branch: (then, left),
                else_branch: (otherwise, right),
            },
        }
    }
}

/// Finish one captured branch under the caller's source-specific store rules.
pub fn complete<S>(
    state: &mut S,
    buffer: impl Fn(&mut S) -> &mut BlockBuilder,
    inner: BlockBuilder,
    finish: impl FnOnce(&mut S),
) -> BlockBuilder {
    let outer = std::mem::replace(buffer(state), inner);
    finish(state);
    std::mem::replace(buffer(state), outer)
}

/// The inverse of folding expression bindings into a return value.
pub fn expanded_return(block: &[Stmt]) -> Option<Block> {
    let [Stmt::Return { value }] = block else {
        return None;
    };
    let mut value = value;
    let mut expanded = Vec::new();
    while let Expr::Let {
        name,
        value: bound,
        body,
    } = value
    {
        expanded.push(Stmt::Let {
            name: name.clone(),
            value: Rhs::Expr(*bound.clone()),
        });
        value = body;
    }
    expanded.push(Stmt::Return {
        value: value.clone(),
    });
    Some(expanded)
}
