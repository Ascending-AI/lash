//! Carrying a parked run onto a rewritten document (`K-VER-005`).
//!
//! A parked run is data in the document's terms: where each task stands is
//! a site, a variable is a name and the node that declares it, a wait is
//! its effect's identity. Carrying it is moving each of those coordinates
//! to where the rewrite put its node. Values, heap objects, waits and
//! meters are carried as they are.

use std::sync::Arc;

use lash_kernel_doc::{
    Datum, Document, EffectIdentity, ErrorDatum, ErrorValue, KernelVersion, Name, Node, Object,
    Site, SpawnIdentity, TaskIdentity, Unit, Value,
};
use lash_kernel_state::{
    Bound, EffectOutcome, Ended, Held, Incoming, ParkedCall, ParkedRun, PerformState, Request,
    Task, TaskState,
};

use crate::migration::{ParkedRefusal, Rewritten};

/// Carries `parked`, a run of `base`, onto `rewritten`, as a run under
/// kernel version `to`.
///
/// Every site the run saves moves to its successor in the rewrite's
/// correspondence, every function identity it pins to the function
/// redeclared for it, and every reference to a declared function to that
/// function's name in the rewritten document. A coordinate with no
/// successor refuses the run, naming it. The names of variables are kept:
/// a rewrite that renames one rewrites the state before it carries it.
pub fn carry(
    parked: &ParkedRun,
    base: &Document,
    rewritten: &Rewritten,
    to: KernelVersion,
) -> Result<ParkedRun, ParkedRefusal> {
    if parked.run.document != rewritten.correspondence.base {
        return Err(ParkedRefusal::Document {
            parked: parked.run.document,
            rewritten: rewritten.correspondence.base,
        });
    }
    let carry = Carry { base, rewritten };
    let mut next = parked.clone();
    next.run.kernel = to.number();
    next.run.document = rewritten.correspondence.result;
    next.run.functions =
        parked
            .run
            .functions
            .iter()
            .map(|function| {
                rewritten.functions.get(function).copied().ok_or(
                    ParkedRefusal::FunctionNotCarried {
                        function: *function,
                    },
                )
            })
            .collect::<Result<_, _>>()?;
    for value in next.session.values_mut() {
        carry.value(value)?;
    }
    for task in &mut next.tasks {
        carry.task(&mut task.handle)?;
        for call in &mut task.calls {
            carry.call(call)?;
        }
    }
    for object in next.objects.values_mut() {
        carry.object(object)?;
    }
    Ok(next)
}

impl Rewritten {
    /// The identity a wait of the old document has in the rewritten one.
    pub fn effect_identity(
        &self,
        base: &Document,
        identity: &EffectIdentity,
    ) -> Result<EffectIdentity, ParkedRefusal> {
        let mut identity = identity.clone();
        Carry {
            base,
            rewritten: self,
        }
        .effect(&mut identity)?;
        Ok(identity)
    }
}

struct Carry<'a> {
    base: &'a Document,
    rewritten: &'a Rewritten,
}

impl Carry<'_> {
    fn site(&self, site: &mut Site, held: &'static str) -> Result<(), ParkedRefusal> {
        match self.rewritten.correspondence.successor(site) {
            Some(to) => {
                *site = to.clone();
                Ok(())
            }
            None => Err(ParkedRefusal::SiteNotCarried {
                site: site.clone(),
                held,
            }),
        }
    }

    /// A call's pending statement: a statement, or one past the last
    /// statement of a block, which stands for the block's end.
    fn statement(&self, site: &mut Site) -> Result<(), ParkedRefusal> {
        let held = "a task's pending statement";
        if self.base.node(site).is_some() || matches!(site.unit, Unit::Library(_)) {
            return self.site(site, held);
        }
        let refused = || ParkedRefusal::SiteNotCarried {
            site: site.clone(),
            held,
        };
        let Some((last, block)) = site.path.split_last() else {
            return Err(refused());
        };
        let mut block = Site::new(site.unit.clone(), block);
        let statements = |document: &Document, block: &Site| match document.node(block) {
            Some(Node::Block(statements)) => u32::try_from(statements.len()).ok(),
            _ => None,
        };
        if statements(self.base, &block) != Some(*last) {
            return Err(refused());
        }
        self.site(&mut block, held)?;
        let end = statements(&self.rewritten.document, &block).ok_or_else(refused)?;
        *site = block.child(end);
        Ok(())
    }

    fn function(&self, name: &mut Name) -> Result<(), ParkedRefusal> {
        let mut body = Site::new(Unit::Function(name.clone()), []);
        self.site(&mut body, "a reference to a declared function")?;
        match body.unit {
            Unit::Function(to) => {
                *name = to;
                Ok(())
            }
            unit => Err(ParkedRefusal::StateNotCarried {
                detail: format!(
                    "declared function `{name}` became {}, which no value references",
                    Site::new(unit, [])
                ),
            }),
        }
    }

    fn value(&self, value: &mut Value) -> Result<(), ParkedRefusal> {
        match value {
            Value::Function(name) => self.function(name),
            Value::Tuple(members) => {
                let mut carried = members.to_vec();
                for member in &mut carried {
                    self.value(member)?;
                }
                *members = carried.into();
                Ok(())
            }
            Value::Error(error) => {
                let mut data = error.data.clone();
                self.value(&mut data)?;
                *error = Arc::new(ErrorValue {
                    kind: error.kind.clone(),
                    message: error.message.clone(),
                    data,
                });
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn datum(&self, datum: &mut Datum) -> Result<(), ParkedRefusal> {
        match datum {
            Datum::Function(name) => self.function(name),
            Datum::Tuple(items) | Datum::List(items) | Datum::Set(items) => {
                items.iter_mut().try_for_each(|item| self.datum(item))
            }
            Datum::Map(entries) => entries.iter_mut().try_for_each(|(key, value)| {
                self.datum(key)?;
                self.datum(value)
            }),
            Datum::Record(fields) => fields
                .iter_mut()
                .try_for_each(|(_, value)| self.datum(value)),
            Datum::Error(error) => self.error(error),
            _ => Ok(()),
        }
    }

    fn error(&self, error: &mut ErrorDatum) -> Result<(), ParkedRefusal> {
        self.datum(&mut error.data)
    }

    fn object(&self, object: &mut Object) -> Result<(), ParkedRefusal> {
        match object {
            Object::List(items) | Object::Set(items) => {
                items.iter_mut().try_for_each(|item| self.value(item))
            }
            Object::Map(entries) => entries.iter_mut().try_for_each(|(key, value)| {
                self.value(key)?;
                self.value(value)
            }),
            Object::Record(fields) => fields
                .iter_mut()
                .try_for_each(|(_, value)| self.value(value)),
            Object::Closure(closure) => self.site(&mut closure.site, "a closure's expression"),
            Object::Variable(value) => self.value(value),
        }
    }

    fn task_identity(&self, identity: &mut TaskIdentity) -> Result<(), ParkedRefusal> {
        match identity {
            TaskIdentity::Main => Ok(()),
            TaskIdentity::Spawned(spawn) => {
                let mut parent = (*spawn.parent).clone();
                self.task_identity(&mut parent)?;
                let mut site = spawn.site.clone();
                self.site(&mut site, "a task's spawn")?;
                *spawn = SpawnIdentity {
                    parent: Arc::new(parent),
                    site,
                    occurrence: spawn.occurrence,
                };
                Ok(())
            }
        }
    }

    fn effect(&self, identity: &mut EffectIdentity) -> Result<(), ParkedRefusal> {
        self.task_identity(&mut identity.task)?;
        self.site(&mut identity.site, "a wait's action")?;
        identity
            .loops
            .iter_mut()
            .try_for_each(|iteration| self.site(&mut iteration.site, "a wait's enclosing loop"))
    }

    fn task(&self, task: &mut Task) -> Result<(), ParkedRefusal> {
        self.task_identity(&mut task.identity)?;
        for occurrence in &mut task.occurrences {
            self.site(&mut occurrence.site, "an action the task has run")?;
        }
        // Occurrences are in site order (`K-SITE-002`), which a rewrite
        // that moves nodes may change.
        task.occurrences.sort_by(|a, b| a.site.cmp(&b.site));
        match &mut task.state {
            TaskState::Ready | TaskState::Joining(_) | TaskState::JoiningMany(_) => Ok(()),
            TaskState::Resuming(Incoming::Joined(_)) => Ok(()),
            TaskState::Resuming(Incoming::Value(value) | Incoming::Raise(value))
            | TaskState::Ended(Ended::Returned(value) | Ended::Raised(value)) => self.value(value),
            TaskState::Performing(perform) => match &mut perform.request {
                Request::Effect {
                    identity, state, ..
                } => {
                    self.effect(identity)?;
                    match state {
                        PerformState::Requested | PerformState::Admitted => Ok(()),
                        PerformState::Committed(EffectOutcome::Completed(datum)) => {
                            self.datum(datum)
                        }
                        PerformState::Committed(EffectOutcome::Failed(error)) => self.error(error),
                    }
                }
                Request::Sleep { identity, .. } => self.effect(identity),
            },
        }
    }

    fn call(&self, parked: &mut ParkedCall) -> Result<(), ParkedRefusal> {
        let call = &mut parked.call;
        self.statement(&mut call.statement)?;
        for binding in &mut call.bindings {
            self.site(&mut binding.declared, "a variable's declaration")?;
            if let Bound::Value(value) = &mut binding.value {
                self.value(value)?;
            }
        }
        for active in &mut call.loops {
            self.site(&mut active.site, "a loop under way")?;
        }
        for cleanup in &mut call.finally {
            self.site(&mut cleanup.site, "a cleanup block under way")?;
        }
        let Held {
            iterated,
            departing,
            arguments,
        } = &mut parked.held;
        iterated
            .iter_mut()
            .chain(departing)
            .chain(arguments.iter_mut().flatten())
            .try_for_each(|value| self.value(value))
    }
}
