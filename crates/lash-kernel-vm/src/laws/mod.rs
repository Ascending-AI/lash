//! Laws of the machine. Each pins a named rule of
//! `docs/kernel/semantics.md`, in kernel text, through the embedder of
//! [`embedder`].

mod await_phase;
mod bounds;
mod chunks;
mod cold;
mod continuation;
pub(crate) mod embedder;
mod forms;
mod frames;
mod identity;
mod layout;
mod observe;
mod operations;
mod operators;
mod paged;
mod regions;
mod resident;
mod stream;
mod tasks;
mod verdict;
