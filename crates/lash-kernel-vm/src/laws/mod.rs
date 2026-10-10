//! Laws of the machine. Each pins a named rule of
//! `docs/kernel/semantics.md`, in kernel text, through the embedder of
//! [`embedder`].

mod bounds;
mod embedder;
mod fast;
mod forms;
mod layout;
mod parked;
mod tasks;
