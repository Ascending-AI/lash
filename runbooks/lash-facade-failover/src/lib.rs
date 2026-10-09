//! The host of the lash-facade-failover runbook (FIG-5193,
//! `runbooks/lash-facade-failover`): lash cores over one PostgreSQL store,
//! each serving its own node, killed, partitioned, stopped and restarted at
//! chosen points, under a witness ledger the cores cannot rewrite.
//!
//! A node ([`node::run`]) is one OS process holding one [`lash::LashCore`]:
//! the core serves the store's session and process actors on its own node,
//! as any lash host's core does. Everything the node uses is the lash
//! facade's. It runs one workload of each kind the cases need:
//!
//! - **a turn** ([`turn`]): a scripted model answers with a TypeScript cell
//!   that calls the `Once` host tool `ext_write`, then with a final answer;
//! - **a process** ([`process`]): a host engine, contributed by a plugin,
//!   that runs an engine step, waits on a pinned key until its deadline,
//!   runs another engine step and ends.
//!
//! Every body and model attempt writes its entry to the witness database
//! before it does anything else ([`witness`]), and the node reports every
//! durable write and its answer on stdout ([`events`], [`recorded`]). The
//! test binary (`tests/failover.rs`) starts the server and the nodes, injects
//! the faults and judges the cases from the witness ledger, the nodes'
//! reports and the store's own rows.

pub mod events;
pub mod node;
pub mod process;
pub mod recorded;
pub mod turn;
pub mod witness;
