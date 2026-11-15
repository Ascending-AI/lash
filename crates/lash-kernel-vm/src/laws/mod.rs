//! Laws of the machine. Each pins a named rule of
//! `docs/kernel/semantics.md`, in kernel text, through the embedder of
//! [`embedder`].

mod admission;
mod await_phase;
mod bounds;
mod chunks;
mod cold;
mod continuation;
mod crossing;
pub(crate) mod embedder;
mod forms;
mod frames;
mod heap;
mod identity;
mod large;
mod layout;
mod limiters;
mod links;
mod observe;
mod operations;
mod operators;
mod paged;
mod pause;
mod pinning;
mod preparation;
mod resident;
mod rewrite;
mod scheduler;
mod session;
mod stream;
mod tasks;
mod verdict;
