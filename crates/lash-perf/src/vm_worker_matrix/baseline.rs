//! The retained in-parent benchmark reference, never a production execution mode.
use super::workload::{Case, Host, environment};
use anyhow::{Result, bail};
use lashlang::{
    ExecutionBounds, ExecutionMode, VmExecutionStart, VmInstance, VmRequest, VmResume, VmRunConfig,
    VmStep,
};
use std::sync::Arc;

fn run_owned(case: &Case) -> Result<()> {
    let mut instance = VmInstance::pristine();
    let mut host = Host::default();
    let mut outcome = None;
    let mode = if case.resumed {
        ExecutionMode::Process
    } else {
        ExecutionMode::Foreground
    };
    for source in &case.cells {
        let program = lash_typescript::parse_cell(source, &environment())
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let spans = program.spans.clone();
        let artifact = lashlang::ModuleArtifact::from_program(program)?;
        let compiled = Arc::new(lashlang::compile(
            &artifact,
            lashlang::Entry::Main,
            Some(&spans),
        )?);
        let config = VmRunConfig::new(mode, ExecutionBounds::unbounded());
        let mut step =
            instance.start(compiled.clone(), VmExecutionStart::Session, config.clone())?;
        loop {
            step = match step {
                VmStep::Suspended(suspended) => {
                    let answer = match suspended.request {
                        VmRequest::Effect(op) => VmResume::Effect(host.perform(op)),
                        VmRequest::CancelCheckpoint(_) => {
                            VmResume::CancelCheckpoint { cancelled: false }
                        }
                        VmRequest::Boundary => VmResume::Park,
                        VmRequest::ParkDeclined(_) => bail!("baseline declined park"),
                    };
                    instance.resume(answer)?
                }
                VmStep::Parked(parked) => {
                    let bytes = parked.continuation.to_bytes()?;
                    instance = VmInstance::pristine();
                    let continuation = instance.open_continuation(&bytes)?;
                    instance.start(
                        compiled.clone(),
                        VmExecutionStart::Continuation(Box::new(continuation)),
                        config.clone(),
                    )?
                }
                VmStep::Complete(complete) => {
                    outcome = Some(complete.outcome);
                    break;
                }
                VmStep::GuestError(error) => {
                    if !case.error {
                        bail!("unexpected guest error: {:?}", error.failure);
                    }
                    break;
                }
            }
        }
    }
    host.check(case, outcome.as_ref())
}

/// The f0 State/execute boundary. Its executor is created outside the timer.
pub struct Reference(tokio::runtime::Runtime);
impl Reference {
    pub fn new() -> Result<Self> {
        Ok(Self(tokio::runtime::Builder::new_current_thread().build()?))
    }
    pub fn run(&self, case: &Case) -> Result<()> {
        if case.resumed {
            return run_owned(case);
        }
        let mut state = lashlang::State::new();
        let host = ImmediateHost(std::sync::Mutex::new(Host::default()));
        let mut outcome = None;
        for source in &case.cells {
            let program = if case.effects == 0 {
                let globals = state.binding_names().map(str::to_string).collect();
                lash_typescript::parse_with_globals(source, &globals)
            } else {
                lash_typescript::parse_cell(source, &environment())
            }
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let spans = program.spans.clone();
            let artifact = lashlang::ModuleArtifact::from_program(program)?;
            let compiled = lashlang::compile(&artifact, lashlang::Entry::Main, Some(&spans))?;
            match self
                .0
                .block_on(lashlang::execute(&compiled, &mut state, &host))
            {
                Ok(value) => outcome = Some(value),
                Err(error) if case.error => {
                    std::hint::black_box(error);
                }
                Err(error) => bail!("unexpected baseline error: {error:?}"),
            }
        }
        host.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .check(case, outcome.as_ref())
    }
}
struct ImmediateHost(std::sync::Mutex<Host>);
impl lashlang::ExecutionHost for ImmediateHost {
    async fn perform(
        &self,
        op: lashlang::AbilityOp,
    ) -> Result<lashlang::AbilityOutcome, lashlang::ExecutionHostError> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .perform(op)
    }
}
