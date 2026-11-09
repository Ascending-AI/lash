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
// The frozen engine's laws: unreachable since the flip; L6 deletes them (FIG-7688).
#[cfg(any())]
mod loop_plans;
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

/// A distinct role identity for law fixtures.
pub(crate) fn role_id(n: u32) -> lash_kernel_state::kernel_internals::logical::RoleId {
    let mut id = [0; 16];
    id[..4].copy_from_slice(&n.to_be_bytes());
    lash_kernel_state::kernel_internals::logical::RoleId(id)
}
