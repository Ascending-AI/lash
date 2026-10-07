//! The harness of the lash-postgres-workers runbook (FIG-5199,
//! `runbooks/lash-postgres-workers`): lash nodes over one PostgreSQL store,
//! killed, partitioned, stopped and restarted at chosen points, under a
//! witness ledger the nodes cannot rewrite.
//!
//! A node ([`node::run`]) is one OS process serving the production runtime
//! (`lash_core::runtime::durable::node::serve`) over PostgreSQL. It runs one
//! workload of each kind the cases need:
//!
//! - **a turn** ([`turn`]): a scripted model answers with a TypeScript cell
//!   that calls the `Once` host operation `ext.write`, then with a final
//!   answer;
//! - **a process** ([`process`]): a host engine that runs a `Once` step,
//!   waits on a pinned key until its deadline, runs another `Once` step and
//!   ends.
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

/// The format set the runbook's sessions are written in.
pub const SESSION_FORMATS: &str = "lash-postgres-workers/1";
