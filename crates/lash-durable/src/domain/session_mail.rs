//! The session's mailbox rows (L3s, FIG-5196): pending inputs, queued work
//! batches and the session's close, as the session actor drains them on
//! every claim (ADR 0132 §3, §12).
//!
//! A producer writes its row and wakes the session actor in its own store
//! transaction; nothing else asks the session for work. The owner reads the
//! open rows with [`DurableReads::session_mailbox`](super::DurableReads::session_mailbox),
//! decides what to admit, and binds it with [`SessionMailWrite::Admit`]
//! inside the commit that admits the run: the bound rows name that run as
//! the owner that took them (`admitted_run` and `admitted_by`).

use lash_sansio::{BatchId, InputId, SessionId, TurnId};

/// The format set a session actor's state is written in, which every
/// producer that creates the actor with its first wake records.
pub const SESSION_ACTOR_FORMATS: &str = "lash.session/1";

/// One open, unbound next-turn input of the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailInput {
    /// The input.
    pub input: InputId,
    /// Its place in the session's ingress order.
    pub enqueue_seq: u64,
    /// The producer's idempotency key, which names the run a host-keyed
    /// input starts.
    pub source_key: Option<String>,
    /// The run spec it carries, by hash; `None` is the default spec.
    pub run_spec_hash: Option<String>,
    /// The turn its submitted delivery addresses, for an active-turn input;
    /// `None` for next-turn input. An active-turn input whose turn is not
    /// the session's unfinished one is next-turn input by rule.
    pub active_turn: Option<TurnId>,
}

/// What a queued work batch asks the session for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MailBatchKind {
    /// Work a turn consumes, such as a process wake.
    Turn,
    /// A session command, applied by a command run.
    Control,
    /// A session command that runs a plugin task as an operation run of its
    /// own.
    Operation,
}

/// One open, unbound queued work batch of the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailBatch {
    /// The batch.
    pub batch: BatchId,
    /// Its place in the session's ingress order.
    pub enqueue_seq: u64,
    /// What it asks for.
    pub kind: MailBatchKind,
    /// Whether it waits for the current turn's commit rather than the
    /// earliest safe boundary.
    pub after_current_turn: bool,
}

/// The session's open mail, as one read saw it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionMailbox {
    /// Whether the session exists and is not deleted.
    pub live: bool,
    /// Whether the session's close began: it admits nothing more.
    pub closing: bool,
    /// Whether its head owes a follow-on, which the turn lane runs before
    /// any other work.
    pub follow_on_owed: bool,
    /// The run its admitted, unfinished work is bound to, if any.
    pub bound_run: Option<TurnId>,
    /// Its open, unbound next-turn inputs, in ingress order.
    pub inputs: Vec<MailInput>,
    /// Its open, unbound queued work batches, in ingress order.
    pub batches: Vec<MailBatch>,
}

/// A session-mail write inside the session actor's owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionMailWrite {
    /// Bind `inputs` and `batches` to `run`: each must still be open and
    /// unbound, or the commit is refused with
    /// [`DomainRefusal::SessionMailMoved`](super::DomainRefusal::SessionMailMoved).
    /// The bound rows name `run` as the owner that took them.
    Admit {
        /// The session.
        session: SessionId,
        /// The run they open.
        run: TurnId,
        /// The inputs, in admission order.
        inputs: Vec<InputId>,
        /// The batches, in admission order.
        batches: Vec<BatchId>,
    },
}
