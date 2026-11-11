//! A labeled statement. The front end gives each label fresh tokens, throws
//! one for a `break label` or `continue label`, and catches it where the
//! labeled statement ends (`lower/labels.rs`):
//!
//! ```text
//! let b = {}
//! let c = {}
//! while .. { try { <body> } catch e {
//!   if same(e, b) { break } else { if same(e, c) { continue } else { throw e } }
//! } }
//! ```
//!
//! A labeled block has only the break token and an empty break arm. A
//! `try` inside a labeled statement rethrows the tokens first:
//! `if same(e, b) { throw e }`.

use std::collections::BTreeMap;

use lash_kernel_doc::{self as k};
use lash_kernel_value::Name;

use super::lift::{Lift, Lifter, temporary};
use super::source::{Stmt, StmtKind};

/// What a token's throw is.
#[derive(Clone, Debug)]
pub(super) struct Exit {
    pub(super) label: String,
    pub(super) continuing: bool,
}

/// The exit handler a `try`'s catch is: its break token, and its continue
/// token when the labeled statement is a loop.
struct Handler<'a> {
    breaks: &'a Name,
    continues: Option<&'a Name>,
}

impl Lifter<'_> {
    /// Every label token of `block`, with the label it exits.
    pub(super) fn label_tokens(&self, block: &[k::Stmt]) -> BTreeMap<Name, Exit> {
        let mut tokens = BTreeMap::new();
        let mut handlers = 0;
        let mut pending = vec![k::Node::Block(block)];
        while let Some(node) = pending.pop() {
            if let k::Node::Stmt(k::Stmt::Try(scope)) = node
                && let Some(handler) = self.handler(scope)
            {
                let label = match handlers {
                    0 => "outer".to_owned(),
                    n => format!("outer{n}"),
                };
                handlers += 1;
                for (token, continuing) in std::iter::once((handler.breaks, false))
                    .chain(handler.continues.map(|token| (token, true)))
                {
                    tokens.insert(
                        token.clone(),
                        Exit {
                            label: label.clone(),
                            continuing,
                        },
                    );
                }
            }
            pending.extend(node.children());
        }
        tokens
    }

    fn handler<'s>(&self, scope: &'s k::TryStmt) -> Option<Handler<'s>> {
        let catch = scope.catch.as_ref()?;
        if scope.finally.is_some() {
            return None;
        }
        let [
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            },
        ] = catch.body.as_slice()
        else {
            return None;
        };
        let breaks = self.token_test(condition, &catch.binding)?;
        let rethrows = |block: &[k::Stmt]| matches!(block, [k::Stmt::Throw { value: k::Expr::Variable(thrown) }] if *thrown == catch.binding);
        match (then_block.as_slice(), else_block.as_slice()) {
            ([], other) if rethrows(other) => Some(Handler {
                breaks,
                continues: None,
            }),
            (
                [k::Stmt::Break],
                [
                    k::Stmt::If {
                        condition,
                        then_block,
                        else_block,
                    },
                ],
            ) if matches!(then_block.as_slice(), [k::Stmt::Continue]) && rethrows(else_block) => {
                Some(Handler {
                    breaks,
                    continues: Some(self.token_test(condition, &catch.binding)?),
                })
            }
            _ => None,
        }
    }

    /// `same(caught, token)` with `token` a temporary: the token.
    fn token_test<'e>(&self, condition: &'e k::Expr, caught: &Name) -> Option<&'e Name> {
        let k::Expr::Call { function, args } = condition else {
            return None;
        };
        let [k::Expr::Variable(left), k::Expr::Variable(token)] = args.as_slice() else {
            return None;
        };
        (self.is(function, "same") && left == caught && temporary(token)).then_some(token)
    }

    /// Whether `statement` is label machinery a labeled statement's own
    /// source gives: a token's binding, or a `try`'s rethrow of one.
    pub(super) fn label_machinery(&self, statement: &k::Stmt) -> bool {
        match statement {
            k::Stmt::Let { name, .. } => self.labels.contains_key(name),
            k::Stmt::If {
                condition: k::Expr::Call { function, args },
                then_block,
                else_block,
            } if else_block.is_empty() && self.is(function, "same") => matches!(
                (args.as_slice(), then_block.as_slice()),
                (
                    [k::Expr::Variable(caught), k::Expr::Variable(token)],
                    [k::Stmt::Throw { value: k::Expr::Variable(thrown) }],
                ) if caught == thrown && self.labels.contains_key(token)
            ),
            _ => false,
        }
    }

    /// A labeled statement's exit handler at `statement`: a block's, as the
    /// labeled block; a loop's, as its body, the loop taking the label.
    pub(super) fn exit_handler(
        &mut self,
        statement: &k::Stmt,
        site: &lash_kernel_value::sites::Site,
    ) -> Lift<Option<Vec<Stmt>>> {
        let k::Stmt::Try(scope) = statement else {
            return Ok(None);
        };
        let Some(handler) = self.handler(scope) else {
            return Ok(None);
        };
        let Some(exit) = self.labels.get(handler.breaks).cloned() else {
            return Ok(None);
        };
        let body = self.block(&scope.body, &site.child(0))?;
        Ok(Some(if handler.continues.is_some() {
            self.loop_label = Some(exit.label);
            body
        } else {
            vec![Stmt {
                caption: None,
                kind: StmtKind::Labeled(exit.label, Box::new(StmtKind::Block(body))),
            }]
        }))
    }

    /// `kind`, a loop, under the label its body's exit handler gave.
    pub(super) fn labeled(&mut self, kind: StmtKind) -> StmtKind {
        match self.loop_label.take() {
            Some(label) => StmtKind::Labeled(label, Box::new(kind)),
            None => kind,
        }
    }

    /// A throw of a label token: the exit it is.
    pub(super) fn exit(&self, value: &k::Expr) -> Option<StmtKind> {
        let k::Expr::Variable(token) = value else {
            return None;
        };
        let exit = self.labels.get(token)?;
        Some(if exit.continuing {
            StmtKind::Continue(Some(exit.label.clone()))
        } else {
            StmtKind::Break(Some(exit.label.clone()))
        })
    }
}
