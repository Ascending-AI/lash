//! A machine's state as the whole binary structures of the checkpoint graph
//! (FIG-5887 §3, §4): a park as a [`KernelWhole`], an end as a
//! [`TerminalWhole`].
//!
//! The machine captures and restores the [`ParkedRun`] it always has; this
//! module is the one place it becomes the persisted form, where a wait loses
//! its request body to the embedder's admission and an end's two binding
//! sets share one object table.

use std::collections::BTreeMap;

use lash_kernel_doc::graph_tokens::ChildRef;
use lash_kernel_doc::{
    Datum, EffectIdentity, FrozenDatum, Identity, Name, Object, ObjectId, Value,
};
use lash_kernel_state::{
    AdmittedRequest, KernelWhole, ParkedRun, TerminalBound, TerminalEnd, TerminalWhole, WholeError,
};

use crate::interface::{
    Bindings, Bound, BoundExceeded, Bounds, End, ExportError, Finished, ImportError, Machine,
    Program, RunError,
};
use crate::machine::KernelMachine;

/// Why a whole state was not written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WholeExportError<E> {
    #[error(transparent)]
    Export(#[from] ExportError),
    #[error(transparent)]
    Whole(WholeError<E>),
}

/// Why a whole state was not resumed.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum WholeImportError<E> {
    #[error(transparent)]
    Whole(WholeError<E>),
    #[error(transparent)]
    Import(#[from] ImportError),
}

/// Why an end was not written or read as a terminal whole.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TerminalError<E> {
    /// The carried and the closure bindings name one identity with two
    /// different bodies.
    #[error("object {0:?} has two different bodies")]
    ObjectConflict(ObjectId),
    /// An object no binding reaches, or a binding that names an object the
    /// table does not hold.
    #[error("the object table and the bindings do not match: object {0:?}")]
    Unreached(ObjectId),
    /// Staging or reading the frozen result failed.
    #[error("a frozen value failed: {0}")]
    Frozen(E),
}

impl KernelMachine {
    /// The run's state at this safe point as a whole structure that pins
    /// `release` and belongs to `owner`. A committed outcome is staged with
    /// `freeze`.
    ///
    /// # Errors
    ///
    /// The machine's export refusal, [`WholeError::Unpublished`] when a wait
    /// has not been handed out at a park, or `freeze`'s error.
    pub fn export_whole<E>(
        &mut self,
        release: ChildRef,
        owner: ChildRef,
        freeze: impl FnMut(&FrozenDatum) -> Result<ChildRef, E>,
    ) -> Result<KernelWhole, WholeExportError<E>> {
        #[cfg(any(test, feature = "perf-witness"))]
        let mut phase = crate::perf_witness::Phase::begin("machine.export");
        let parked = self.export()?;
        #[cfg(any(test, feature = "perf-witness"))]
        {
            if let Some(phase) = &mut phase {
                phase.complete(
                    parked.tasks.len(),
                    parked.tasks.iter().map(|task| task.calls.len()).sum(),
                    parked.objects.len(),
                );
            }
            drop(phase);
        }
        #[cfg(any(test, feature = "perf-witness"))]
        let mut phase = crate::perf_witness::Phase::begin("whole.capture");
        let whole = KernelWhole::capture(&parked, release, owner, freeze)
            .map_err(WholeExportError::Whole)?;
        #[cfg(any(test, feature = "perf-witness"))]
        if let Some(phase) = &mut phase {
            phase.complete(
                whole.tasks.len(),
                whole.tasks.iter().map(|task| task.calls.len()).sum(),
                whole.objects.len(),
            );
        }
        Ok(whole)
    }

    /// Rebuilds a run from a whole state. Each wait's request is the one
    /// `admitted` holds for its identity; each committed outcome is read
    /// with `thaw`. The library functions the run pins are its document's.
    ///
    /// # Errors
    ///
    /// A wait no admission matches, `thaw`'s error, or the machine's import
    /// refusal.
    pub fn import_whole<E>(
        program: Program,
        bounds: Bounds,
        whole: KernelWhole,
        thaw: impl FnMut(ChildRef) -> Result<Datum, E>,
        admitted: impl FnMut(&EffectIdentity) -> Option<AdmittedRequest>,
    ) -> Result<Self, WholeImportError<E>> {
        let functions = program
            .document
            .manifest
            .functions
            .keys()
            .copied()
            .collect();
        #[cfg(any(test, feature = "perf-witness"))]
        let mut phase = crate::perf_witness::Phase::begin("whole.restore");
        let parked: ParkedRun = whole
            .restore(functions, thaw, admitted)
            .map_err(WholeImportError::Whole)?;
        #[cfg(any(test, feature = "perf-witness"))]
        let geometry = (
            parked.tasks.len(),
            parked.tasks.iter().map(|task| task.calls.len()).sum(),
            parked.objects.len(),
        );
        #[cfg(any(test, feature = "perf-witness"))]
        {
            if let Some(phase) = &mut phase {
                phase.complete(geometry.0, geometry.1, geometry.2);
            }
            drop(phase);
        }
        #[cfg(any(test, feature = "perf-witness"))]
        let mut phase = crate::perf_witness::Phase::begin("machine.import");
        let machine = Self::import(program, bounds, parked)?;
        #[cfg(any(test, feature = "perf-witness"))]
        if let Some(phase) = &mut phase {
            phase.complete(geometry.0, geometry.1, geometry.2);
        }
        Ok(machine)
    }
}

/// `end` as a terminal whole; `None` for a cancelled run, which records no
/// end. The result, a failure's reason or an uncaught error is staged with
/// `freeze`.
///
/// # Errors
///
/// [`TerminalError::ObjectConflict`] when the two binding sets disagree on
/// an object, or `freeze`'s error.
pub fn terminal_whole<E>(
    end: &End,
    mut freeze: impl FnMut(&FrozenDatum) -> Result<ChildRef, E>,
) -> Result<Option<TerminalWhole>, TerminalError<E>> {
    let mut frozen = |datum: &Datum| {
        freeze(&FrozenDatum(datum.clone()))
            .map(Some)
            .map_err(TerminalError::Frozen)
    };
    let mut whole = TerminalWhole {
        end: TerminalEnd::Finished,
        result: None,
        finish: false,
        carried: BTreeMap::new(),
        closures: BTreeMap::new(),
        objects: BTreeMap::new(),
        not_carried: Vec::new(),
    };
    match end {
        End::Finished(finished) => {
            whole.result = frozen(&finished.result)?;
            whole.finish = finished.finish;
            whole.carried.clone_from(&finished.bindings.variables);
            whole.closures.clone_from(&finished.closures.variables);
            whole.not_carried.clone_from(&finished.not_carried);
            for (id, object) in finished
                .bindings
                .objects
                .iter()
                .chain(&finished.closures.objects)
            {
                match whole.objects.get(id) {
                    Some(held) if held != object => return Err(TerminalError::ObjectConflict(*id)),
                    Some(_) => {}
                    None => {
                        whole.objects.insert(*id, object.clone());
                    }
                }
            }
        }
        End::Failed(reason) => {
            whole.end = TerminalEnd::Failed;
            whole.result = frozen(reason)?;
        }
        End::Error(RunError::Uncaught(error)) => {
            whole.end = TerminalEnd::Uncaught;
            whole.result = frozen(error)?;
        }
        End::Error(RunError::TasksOutstanding {
            unfinished,
            unobserved,
        }) => {
            whole.end = TerminalEnd::TasksOutstanding {
                unfinished: unfinished.clone(),
                unobserved: unobserved.clone(),
            };
        }
        End::Error(RunError::Deadlock { waiting }) => {
            whole.end = TerminalEnd::Deadlock {
                waiting: waiting.clone(),
            };
        }
        End::Error(RunError::Bound(exceeded)) => {
            whole.end = TerminalEnd::Bound {
                bound: match &exceeded.bound {
                    Bound::Charge => TerminalBound::Charge,
                    Bound::Memory => TerminalBound::Memory,
                    Bound::CallDepth => TerminalBound::CallDepth,
                    Bound::LiveTasks => TerminalBound::LiveTasks,
                    Bound::RequestsPerPark => TerminalBound::RequestsPerPark,
                    Bound::JoinMembers => TerminalBound::JoinMembers,
                    Bound::Guard { function } => TerminalBound::Guard {
                        function: *function,
                    },
                },
                limit: exceeded.limit,
                function: exceeded.function.clone(),
            };
        }
        End::Cancelled => return Ok(None),
    }
    Ok(Some(whole))
}

/// The end a terminal whole records. The one object table is split back
/// into what the carried bindings reach and what the closure bindings
/// reach; every object is reached by one or both.
///
/// # Errors
///
/// [`TerminalError::Unreached`] for an object no binding reaches or a
/// binding naming an object the table lacks, or `thaw`'s error.
pub fn terminal_end<E>(
    whole: TerminalWhole,
    mut thaw: impl FnMut(ChildRef) -> Result<Datum, E>,
) -> Result<End, TerminalError<E>> {
    let mut result = || match whole.result {
        Some(result) => thaw(result).map_err(TerminalError::Frozen),
        None => Ok(Datum::Null),
    };
    Ok(match whole.end {
        TerminalEnd::Finished => {
            let result = result()?;
            let bindings = reached(&whole.carried, &whole.objects)?;
            let closures = reached(&whole.closures, &whole.objects)?;
            if let Some(id) = whole
                .objects
                .keys()
                .find(|id| !bindings.contains_key(id) && !closures.contains_key(id))
            {
                return Err(TerminalError::Unreached(*id));
            }
            End::Finished(Finished {
                result,
                finish: whole.finish,
                bindings: Bindings {
                    variables: whole.carried,
                    objects: bindings,
                },
                not_carried: whole.not_carried,
                closures: Bindings {
                    variables: whole.closures,
                    objects: closures,
                },
            })
        }
        TerminalEnd::Failed => End::Failed(result()?),
        TerminalEnd::Uncaught => End::Error(RunError::Uncaught(result()?)),
        TerminalEnd::TasksOutstanding {
            unfinished,
            unobserved,
        } => End::Error(RunError::TasksOutstanding {
            unfinished,
            unobserved,
        }),
        TerminalEnd::Deadlock { waiting } => End::Error(RunError::Deadlock { waiting }),
        TerminalEnd::Bound {
            bound,
            limit,
            function,
        } => End::Error(RunError::Bound(BoundExceeded {
            bound: match bound {
                TerminalBound::Charge => Bound::Charge,
                TerminalBound::Memory => Bound::Memory,
                TerminalBound::CallDepth => Bound::CallDepth,
                TerminalBound::LiveTasks => Bound::LiveTasks,
                TerminalBound::RequestsPerPark => Bound::RequestsPerPark,
                TerminalBound::JoinMembers => Bound::JoinMembers,
                TerminalBound::Guard { function } => Bound::Guard { function },
            },
            limit,
            function,
        })),
    })
}

/// The objects `variables` reach in `objects`, through tuples, errors,
/// refs, collections, closures and the variables they share.
fn reached<E>(
    variables: &BTreeMap<Name, Value>,
    objects: &BTreeMap<ObjectId, Object>,
) -> Result<BTreeMap<ObjectId, Object>, TerminalError<E>> {
    let mut out = BTreeMap::new();
    let mut pending = Vec::new();
    for value in variables.values() {
        named(value, &mut pending);
    }
    while let Some(id) = pending.pop() {
        if out.contains_key(&id) {
            continue;
        }
        let object = objects.get(&id).ok_or(TerminalError::Unreached(id))?;
        match object {
            Object::List(items) | Object::Set(items) => {
                items.iter().for_each(|item| named(item, &mut pending));
            }
            Object::Map(entries) => entries.iter().for_each(|(key, value)| {
                named(key, &mut pending);
                named(value, &mut pending);
            }),
            Object::Record(fields) => fields
                .iter()
                .for_each(|(_, value)| named(value, &mut pending)),
            Object::Closure(closure) => {
                pending.extend(closure.captures.iter().map(|(_, cell)| *cell));
            }
            Object::Variable(value) => named(value, &mut pending),
        }
        out.insert(id, object.clone());
    }
    Ok(out)
}

fn named(value: &Value, out: &mut Vec<ObjectId>) {
    match value {
        Value::Tuple(members) => members.iter().for_each(|member| named(member, out)),
        Value::Error(error) => named(&error.data, out),
        Value::Ref(Identity::Object(id)) => out.push(*id),
        other => out.extend(other.object()),
    }
}
