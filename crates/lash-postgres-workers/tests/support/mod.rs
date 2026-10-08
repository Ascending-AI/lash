//! The failover cases' harness: the lash database (a PostgreSQL server the
//! case owns, or a SQLite file), and the node processes over it.

pub mod cluster;
pub mod postgres;
