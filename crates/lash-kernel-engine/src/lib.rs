//! Standalone compilation and resumable execution over the Lash kernel.
//! See the crate README for external-effect ledger responsibilities.
use lash_kernel_doc::heap::PageId;
pub use lash_kernel_doc::{Datum, ErrorDatum};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Machine, Outcome, Request, Step, Target, WaitId,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

mod compile;
pub mod embedding;
mod native;
mod pages;
mod value;
pub use lash_kernel_dialect::Diagnostic;
pub use lash_kernel_doc::{Bytes, Float, Integer, Timestamp};
pub use lash_kernel_value::{ContractId, Name, Param, Signature, Type};
pub use lash_kernel_vm::RunError;
pub use value::{Value, ValueError};

mod native_tables {
    include!(concat!(env!("OUT_DIR"), "/native_tables.rs"));
}

#[derive(Clone, Copy, Debug)]
pub enum Dialect {
    TypeScript,
    Python,
}

#[derive(Clone, Debug)]
pub struct External {
    pub name: String,
    pub contract: ContractId,
    pub signature: Signature,
}

/// Finite policy for a run. Snapshots retain it, including cumulative output.
#[derive(Clone, Debug)]
pub struct Limits {
    pub charge: u64,
    pub memory_bytes: u64,
    pub call_depth: u32,
    pub live_tasks: u32,
    pub calls_per_park: u32,
    pub join_members: u32,
    pub source_bytes: usize,
    pub source_nesting: u32,
    pub value_bytes: usize,
    pub output_bytes: usize,
    pub storage_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            charge: 1_000_000,
            memory_bytes: 64 << 20,
            call_depth: Bounds::DEFAULT_CALL_DEPTH,
            live_tasks: 256,
            calls_per_park: 256,
            join_members: 256,
            source_bytes: 64 << 10,
            source_nesting: 28,
            value_bytes: 4 << 20,
            output_bytes: 4 << 20,
            storage_bytes: 128 << 20,
        }
    }
}
impl Limits {
    fn bounds(&self) -> Bounds {
        Bounds {
            memory_meter: lash_kernel_vm::MemoryMeter::On,
            charge: lash_kernel_vm::Charge::Exact,
            charge_limit: self.charge,
            memory: self.memory_bytes,
            call_depth: self.call_depth,
            live_tasks: self.live_tasks,
            requests_per_park: self.calls_per_park,
            join_members: self.join_members,
        }
    }
    fn admits(&self, saved: &Self) -> bool {
        self.charge >= saved.charge
            && self.memory_bytes >= saved.memory_bytes
            && self.call_depth >= saved.call_depth
            && self.live_tasks >= saved.live_tasks
            && self.calls_per_park >= saved.calls_per_park
            && self.join_members >= saved.join_members
            && self.source_bytes >= saved.source_bytes
            && self.source_nesting >= saved.source_nesting
            && self.value_bytes >= saved.value_bytes
            && self.output_bytes >= saved.output_bytes
            && self.storage_bytes >= saved.storage_bytes
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Fault {
    #[error("invalid engine configuration: {0}")]
    Config(String),
    #[error(transparent)]
    Install(#[from] embedding::InstallError),
    #[error(transparent)]
    Value(#[from] ValueError),
    #[error("kernel infrastructure failed: {0}")]
    Kernel(String),
    #[error("unknown or already answered call id")]
    UnknownAnswer,
    #[error("duplicate call id in answer batch")]
    DuplicateAnswer,
    #[error("snapshot or program limits exceed this engine's policy")]
    Policy,
    #[error("cumulative printed output exceeds the configured bound")]
    OutputLimit,
}

pub struct Engine {
    limits: Limits,
    installed: embedding::Installed,
}
#[derive(Clone)]
pub struct Program {
    kernel: lash_kernel_vm::Program,
    target: Target,
    limits: Limits,
}

/// Opaque run-scoped identity retaining both the semantic effect and wait.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CallId {
    run: RunId,
    wait: WaitId,
    effect: lash_kernel_value::sites::EffectId,
}

/// An allocation owned by the run and every call identity it issued.
/// Its address cannot be reused while any of those identities survives.
#[derive(Clone, Debug)]
struct RunId(Arc<u8>);
impl PartialEq for RunId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for RunId {}
impl PartialOrd for RunId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for RunId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        Arc::as_ptr(&self.0).cmp(&Arc::as_ptr(&other.0))
    }
}
#[derive(Clone, Debug)]
pub struct Call {
    pub id: CallId,
    pub name: String,
    pub args: Vec<Value>,
}
#[derive(Clone, Debug)]
pub struct GuestError {
    pub kind: String,
    pub message: String,
    pub data: Value,
}
pub struct Answer {
    pub id: CallId,
    pub result: Result<Value, GuestError>,
}

pub struct Turn {
    pub prints: Vec<Value>,
    pub state: Progress,
}
pub enum Progress {
    Complete(Result<Value, CompletionError>),
    Paused {
        calls: Vec<Call>,
        withdrawn: Vec<CallId>,
        snapshot: Snapshot,
    },
}
#[derive(Clone, Debug, thiserror::Error)]
pub enum CompletionError {
    #[error(transparent)]
    Run(#[from] RunError),
    #[error("guest failed: {0:?}")]
    Failed(Value),
    #[error("run cancelled")]
    Cancelled,
}

/// Same-process continuation. There is no archive or cross-version contract.
/// Consuming a snapshot prevents accidental double resume through this API.
pub struct Snapshot {
    inner: Box<Continuation>,
}
struct Continuation {
    program: Program,
    checkpoint: lash_kernel_state::paged::Checkpoint,
    pages: pages::Pages,
    run: RunId,
    pending: Vec<Call>,
    retired: BTreeSet<CallId>,
    output_bytes: usize,
}
impl Snapshot {
    pub fn pending(&self) -> &[Call] {
        &self.inner.pending
    }
}

impl Engine {
    pub fn new(limits: Limits) -> Result<Self, Fault> {
        if limits.charge == 0
            || limits.memory_bytes == 0
            || limits.call_depth == 0
            || limits.live_tasks == 0
            || limits.calls_per_park == 0
            || limits.join_members == 0
            || limits.source_nesting == 0
            || limits.source_bytes == 0
            || limits.value_bytes == 0
            || limits.storage_bytes == 0
        {
            return Err(Fault::Config(
                "execution and admission limits must be positive".into(),
            ));
        }
        Ok(Self {
            limits,
            installed: embedding::standard()?,
        })
    }
    pub fn run(&self, program: &Program, args: Vec<Value>) -> Result<Turn, Fault> {
        if !self.limits.admits(&program.limits) {
            return Err(Fault::Policy);
        }
        check_values(&args, program.limits.value_bytes)?;
        let run = RunId(Arc::new(0));
        let pages = pages::Pages::new(program.limits.storage_bytes);
        let machine = KernelMachine::start(
            program.kernel.clone(),
            program.limits.bounds(),
            CellStart {
                target: program.target.clone(),
                args: args.into_iter().map(Value::into_data).collect(),
                session: None,
                owner: PageId([0; 32]),
                modes: BTreeMap::new(),
                host: Vec::new(),
            },
            Box::new(pages.clone()),
        )
        .map_err(kernel_fault)?;
        self.drive(
            machine,
            program.clone(),
            pages,
            run,
            Vec::new(),
            BTreeSet::new(),
            0,
        )
    }
    pub fn resume(&self, snapshot: Snapshot, answers: Vec<Answer>) -> Result<Turn, Fault> {
        let snapshot = snapshot.inner;
        if !self.limits.admits(&snapshot.program.limits) {
            return Err(Fault::Policy);
        }
        let mut seen = BTreeSet::new();
        for answer in &answers {
            if !seen.insert(answer.id.clone()) {
                return Err(Fault::DuplicateAnswer);
            }
            if !snapshot.pending.iter().any(|call| call.id == answer.id)
                && !snapshot.retired.contains(&answer.id)
            {
                return Err(Fault::UnknownAnswer);
            }
            // Withdrawn answers are discarded, including their payloads.
            if snapshot.retired.contains(&answer.id) {
                continue;
            }
            match &answer.result {
                Ok(value) => check_values(
                    std::slice::from_ref(value),
                    snapshot.program.limits.value_bytes,
                )?,
                Err(error) => {
                    let bytes = error
                        .data
                        .bytes()
                        .checked_add(std::mem::size_of::<Datum>())
                        .and_then(|n| n.checked_add(error.kind.len()))
                        .and_then(|n| n.checked_add(error.message.len()))
                        .ok_or(ValueError::Limit)?;
                    if bytes > snapshot.program.limits.value_bytes {
                        return Err(ValueError::Limit.into());
                    }
                }
            }
        }
        let mut machine = KernelMachine::open(
            snapshot.program.kernel.clone(),
            snapshot.program.limits.bounds(),
            &snapshot.checkpoint,
            Box::new(snapshot.pages.clone()),
        )
        .map_err(kernel_fault)?;
        let mut pending = snapshot.pending;
        for answer in answers {
            if snapshot.retired.contains(&answer.id) {
                continue;
            }
            let outcome = match answer.result {
                Ok(value) => Outcome::Completed(value.into_data()),
                Err(error) => Outcome::Failed(ErrorDatum {
                    kind: error.kind,
                    message: error.message,
                    data: error.data.into_data(),
                }),
            };
            machine
                .deliver(answer.id.wait, outcome)
                .map_err(kernel_fault)?;
            pending.retain(|call| call.id != answer.id);
        }
        self.drive(
            machine,
            snapshot.program,
            snapshot.pages,
            snapshot.run,
            pending,
            snapshot.retired,
            snapshot.output_bytes,
        )
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "the driver owns the continuation and its host ledger together"
    )]
    fn drive(
        &self,
        mut machine: KernelMachine,
        program: Program,
        pages: pages::Pages,
        run: RunId,
        mut pending: Vec<Call>,
        mut retired: BTreeSet<CallId>,
        output_bytes: usize,
    ) -> Result<Turn, Fault> {
        let mut host = Output {
            values: Vec::new(),
            remaining: program.limits.output_bytes.saturating_sub(output_bytes),
            value_limit: program.limits.value_bytes,
            failed: false,
        };
        loop {
            let step = machine.run(&mut host, 10_000).map_err(kernel_fault)?;
            if host.failed {
                return Err(Fault::OutputLimit);
            }
            let state = match step {
                Step::Slice => continue,
                Step::Ended(end) => Progress::Complete(match end {
                    End::Finished(end) => Ok(Value::new(end.result, program.limits.value_bytes)?),
                    End::Failed(value) => Err(CompletionError::Failed(Value::new(
                        value,
                        program.limits.value_bytes,
                    )?)),
                    End::Error(error) => {
                        if let RunError::Uncaught { error: value, .. } = &error {
                            Value::copy(value, program.limits.value_bytes)?;
                        }
                        Err(CompletionError::Run(error))
                    }
                    End::Cancelled => Err(CompletionError::Cancelled),
                }),
                Step::Parked(park) => {
                    let mut calls = Vec::new();
                    for Request::Effect(request) in park.requests {
                        let import =
                            program
                                .kernel
                                .operation_import(&request.use_)
                                .ok_or_else(|| {
                                    Fault::Kernel("operation declaration is absent".into())
                                })?;
                        let args = request
                            .args
                            .into_iter()
                            .map(|arg| Value::new(arg, program.limits.value_bytes))
                            .collect::<Result<Vec<_>, _>>()?;
                        check_values(&args, program.limits.value_bytes)?;
                        let call = Call {
                            id: CallId {
                                run: run.clone(),
                                wait: request.wait,
                                effect: request.identity,
                            },
                            name: import.name.to_string(),
                            args,
                        };
                        pending.push(call.clone());
                        calls.push(call);
                    }
                    let mut withdrawn = Vec::new();
                    pending.retain(|call| {
                        if park.withdrawn.contains(&call.id.wait) {
                            withdrawn.push(call.id.clone());
                            retired.insert(call.id.clone());
                            false
                        } else {
                            true
                        }
                    });
                    let checkpoint = machine.checkpoint().map_err(kernel_fault)?.checkpoint();
                    let output_bytes = program.limits.output_bytes - host.remaining;
                    Progress::Paused {
                        calls,
                        withdrawn,
                        snapshot: Snapshot {
                            inner: Box::new(Continuation {
                                program,
                                checkpoint,
                                pages,
                                run,
                                pending,
                                retired,
                                output_bytes,
                            }),
                        },
                    }
                }
            };
            return Ok(Turn {
                prints: host.values,
                state,
            });
        }
    }
}
fn kernel_fault(error: impl std::fmt::Display) -> Fault {
    Fault::Kernel(error.to_string())
}
fn check_values(values: &[Value], limit: usize) -> Result<(), ValueError> {
    values
        .iter()
        .try_fold(0usize, |bytes, value| {
            bytes
                .checked_add(value.bytes())
                .filter(|sum| *sum <= limit)
                .ok_or(ValueError::Limit)
        })
        .map(|_| ())
}
struct Output {
    values: Vec<Value>,
    remaining: usize,
    value_limit: usize,
    failed: bool,
}
impl Host for Output {
    fn print(&mut self, data: &Datum) {
        match Value::copy(data, self.remaining.min(self.value_limit)) {
            Ok(value) => {
                self.remaining -= value.bytes();
                self.values.push(value);
            }
            Err(_) => self.failed = true,
        }
    }
    fn cancel_requested(&mut self) -> bool {
        self.failed
    }
}
