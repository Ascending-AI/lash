//! The broker for kernel runs (`lash-kernel-vm`).
//!
//! A kernel run is a set of tasks, so it waits on a set of effects. This
//! module is the durable half of that: the machine parks, the broker
//! commits (kernel spec §9 rule 4).
//!
//! - [`ledger`]: the set of waits a saved state stands on, keyed by the
//!   machine's effect identity, and which records stay reachable for it.
//! - [`store`]: the park transaction (saved state and the admission of
//!   every effect requested since the last park, together), delivery of
//!   committed outcomes in any order, and settlement over tasks.
//! - [`run`]: the loop that drives a machine through its parks, in this
//!   process or in a worker, and resumes one from its checkpoint.
//! - [`value`]: effect values as JSON text, every number carried as
//!   written.

pub mod ledger;
pub mod run;
pub mod store;
pub mod value;

pub use ledger::{
    AdmittedEffect, EffectLedger, LedgerRefusal, ParkedCheckpoint, PendingEffect, RecordedBound,
    RecordedEnd, Standing,
};
pub use run::{
    DrivenMachine, InProcess, InProcessMachine, KernelBroker, KernelCeilings, KernelEffects,
    KernelEnd, KernelFailure, Machines, park_bound,
};
pub use store::{
    AdmitAs, EFFECT_CANCELLED, EFFECT_FAILED, EFFECT_INTERRUPTED, EFFECT_RESULT, EFFECT_TIMED_OUT,
    EffectAdmission, EndSave, OutcomeOf, ParkSave, Saved, Settled, outcome_of,
};
pub use value::{InvalidJson, JsonlessKind, NotJson, datum_from_json, datum_to_json};
