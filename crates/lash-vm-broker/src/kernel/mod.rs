//! The broker for kernel runs (`lash-kernel-vm`).
//!
//! A kernel run is a set of tasks, so it waits on a set of effects. This
//! module is the durable half of that: the machine parks, the broker
//! commits (kernel spec §9 rule 4).
//!
//! - [`ledger`]: the set of waits a saved state stands on, keyed by the
//!   machine's effect identity, and which records stay reachable for it.
//! - [`graph`]: a run's admissions, membership, root and end as checkpoint
//!   graph nodes.
//! - [`pages`]: an activation's page service: its root lease, capability
//!   key and spool, and the candidate a park publishes from it.
//! - [`state`]: a checkpoint root's header, the build's releases and a
//!   run's recorded end, as graph nodes.
//! - [`store`]: the park transaction (saved state and the admission of
//!   every effect requested since the last park, together), delivery of
//!   committed outcomes in any order, and settlement over tasks.
//! - [`run`]: the loop that drives a machine through its parks, in this
//!   process or in a worker, and resumes one from its checkpoint.
//! - [`value`]: effect values as JSON text, every number carried as
//!   written.

pub mod graph;
pub mod ledger;
pub mod pages;
pub mod run;
pub mod state;
pub mod store;
pub mod value;

pub use ledger::{AdmittedEffect, EffectLedger, LedgerRefusal, PendingEffect, Standing};
pub use run::{
    DrivenMachine, KernelBroker, KernelCeilings, KernelEffects, KernelEnd, KernelFailure,
    MachinePark, MachineStep, Machines, PageActivation, PagePlan, end_activation, next_stage_lease,
    park_bound,
};
pub use state::EndRecord;
pub use store::{
    AdmitAs, AdmittedCall, Checkpoint, EFFECT_CANCELLED, EFFECT_FAILED, EFFECT_INTERRUPTED,
    EFFECT_RESULT, EFFECT_TIMED_OUT, EffectAdmission, EndSave, FirstPark, HostParts, OutcomeOf,
    ParkSave, ParkedState, RootRead, Saved, SealedPark, Settled, outcome_of,
};
pub use value::{
    InvalidJson, JsonlessKind, NotJson, datum_from_json, datum_from_json_with_limits,
    datum_to_json, datum_to_json_with_limits,
};
