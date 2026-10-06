//! Process actors, cancel, terminal and cascade: the neutral statements of the `processes` domain (I0, FIG-5194).
//!
//! Owned by L6 (FIG-5175): its statements, and its table's DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`).
