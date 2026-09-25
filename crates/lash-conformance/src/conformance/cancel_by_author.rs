//! Cancel by author (ADR 0101 §10, FIG-3543): a cancelled turn disposes of
//! every item it withheld by who authored it, records each one, and the next
//! turn delivers what was deferred exactly once.
//!
//! The fixture is the drive-admission one: a guard, a prefix, the tier's
//! effect host, the store set under test and its
//! [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
//!
//! Owed here, registered through [`cancel_by_author_tests!`](crate::cancel_by_author_tests):
//! a wake withheld at `BeforeCompletion` by a turn cancelled `Immediate` with
//! the host disposition `Drop` is deferred (its row open at the same
//! `enqueue_seq`, the redelivery floor unchanged), is recorded with the
//! disposition `Defer`, and is delivered once by the next turn.
