//! Laws of the machine. Each pins a named rule of
//! `docs/kernel/semantics.md`, in kernel text, through the embedder of
//! [`embedder`].

mod bounds;
mod census;
mod continuation;
pub(crate) mod embedder;
mod forms;
mod frames;
mod layout;
mod observe;
mod paged;
mod publish;
mod stream;
mod tasks;
mod text_projection;
mod verdict;
