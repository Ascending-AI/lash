//! Fixed recurrence graphs. These descriptors have named graph positions,
//! not an instruction array. Other graphs use the ordinary packet stream.

use super::super::compiled::{OperatorSite, operator_site};
use super::*;
use crate::compile::expr::{Expr, Kind as Expression};
use crate::compile::islands::{Kind as StatementKind, Statement};
use crate::compile::{Event, Head, LoopPlan, RegionSlot, Step, Summary};

mod form;
mod verify;

/// Values read by the fixed graph. Invariants cannot be written by its body.
#[derive(Clone, Copy, Debug)]
pub(in crate::machine::islands) enum Input {
    Counter,
    Previous,
    Accumulator,
    Literal(Value),
    Invariant(u32),
}

/// The canonical primitive and its scalar fee table, never a private price.
pub(in crate::machine::islands) struct Scalar {
    pub(in crate::machine::islands) site: OperatorSite,
}

pub(in crate::machine::islands) struct Charge {
    pub(in crate::machine::islands) fixed: u64,
    pub(in crate::machine::islands) events: Box<[Event]>,
    formulas: usize,
}

impl Charge {
    fn of(source: &Summary, formulas: usize) -> Option<Self> {
        if source.scalar_root().is_none()
            || source
                .program
                .iter()
                .any(|event| !matches!(event, Event::Fixed(_) | Event::Formula))
            || source
                .program
                .iter()
                .filter(|event| **event == Event::Formula)
                .count()
                != formulas
        {
            return None;
        }
        Some(Self {
            fixed: source.fixed,
            events: source.program.clone(),
            formulas,
        })
    }

    fn verifies(&self, source: &Summary) -> bool {
        source.scalar_root().is_some()
            && self.fixed == source.fixed
            && self.events == source.program
            && self.formulas
                == source
                    .program
                    .iter()
                    .filter(|event| **event == Event::Formula)
                    .count()
    }
}

pub(in crate::machine::islands) struct Binary {
    pub(in crate::machine::islands) scalar: Scalar,
    pub(in crate::machine::islands) inputs: [Input; 2],
}

pub(in crate::machine::islands) struct Copy {
    pub(in crate::machine::islands) at: u32,
    pub(in crate::machine::islands) keep: bool,
    pub(in crate::machine::islands) unary: Option<Scalar>,
    pub(in crate::machine::islands) charge: Charge,
}

pub(in crate::machine::islands) struct Update {
    pub(in crate::machine::islands) at: u32,
    pub(in crate::machine::islands) operation: Binary,
    pub(in crate::machine::islands) charge: Charge,
}

pub(in crate::machine::islands) struct Reduction {
    pub(in crate::machine::islands) at: u32,
    pub(in crate::machine::islands) scale: Option<Binary>,
    pub(in crate::machine::islands) fold: Scalar,
    pub(in crate::machine::islands) term: Input,
    pub(in crate::machine::islands) reverse: bool,
    pub(in crate::machine::islands) charge: Charge,
}

pub(in crate::machine::islands) struct Repeat {
    pub(in crate::machine::islands) source_room: std::sync::Arc<crate::compile::room::Source>,
    pub(in crate::machine::islands) source_site: lash_kernel_value::sites::Site,
    pub(in crate::machine::islands) charged: bool,
    pub(in crate::machine::islands) test: Binary,
    pub(in crate::machine::islands) test_charge: Charge,
    pub(in crate::machine::islands) before: Option<Copy>,
    pub(in crate::machine::islands) reduction: Option<Reduction>,
    pub(in crate::machine::islands) update: Update,
    pub(in crate::machine::islands) after: Option<Copy>,
}

impl Scalar {
    fn of(expr: &Expr, exe: &Executable, comparison: bool) -> Option<Self> {
        let site = operator_site(expr, exe)?;
        if site.counted.is_some() || site.act.is_some() || site.root.is_some() {
            return None;
        }
        let Expression::Operator { args, closes, .. } = &expr.kind else {
            return None;
        };
        if closes.len() > 1 {
            return None;
        }
        match args.len() {
            1 if !comparison => {}
            2 => {
                let pair = site.pair(exe)?;
                if ![pair.ints, pair.floats]
                    .into_iter()
                    .flatten()
                    .any(|op| op.compares() == comparison)
                {
                    return None;
                }
            }
            _ => return None,
        }
        Some(Self { site })
    }
}

fn unpin(mut expr: &Expr) -> &Expr {
    while let Expression::Pin(value) = &expr.kind {
        expr = value;
    }
    expr
}

fn local(expr: &Expr) -> Option<u32> {
    match unpin(expr).kind {
        Expression::Local {
            local,
            scoped: false,
            ..
        } => Some(local.at),
        _ => None,
    }
}

fn write(statement: &Statement) -> Option<(u32, bool, &Expr)> {
    if statement.root.is_some() {
        return None;
    }
    match &statement.kind {
        StatementKind::Write {
            at, bind, value, ..
        } => Some((*at, *bind, unpin(value))),
        _ => None,
    }
}

fn summary(step: &Step) -> Option<(&Summary, bool)> {
    match step {
        Step::Write(summary) => Some((summary, false)),
        Step::Bind(summary) => Some((summary, true)),
        _ => None,
    }
}
