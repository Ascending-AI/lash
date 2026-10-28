//! Statement and source-note ownership for dialect lowering.
use crate::Note;
use lash_kernel_doc::{Block, Expr, Node, Rhs, Stmt};

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
