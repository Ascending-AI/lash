//! The session's mailbox rows (L3s, FIG-5196): pending inputs, queued work
//! batches and the session's close, as the session actor drains them on
//! every claim (ADR 0132 §3, §12).
//!
//! A producer writes its row and wakes the session actor in its own store
//! transaction; nothing else asks the session for work. The owner reads the
//! open rows with [`DurableReads::session_mailbox`](super::DurableReads::session_mailbox),
//! decides what to admit, and binds it with [`SessionMailWrite::Admit`]
//! inside the commit that admits the run: the bound rows name that run as
//! the owner that took them (`admitted_run` and `admitted_by`). The owner
//! mails its own session with [`SessionMailWrite::Enqueue`]: a frame switch
//! commits its follow-on that way, in its `turn.commit`.

use lash_sansio::{BatchId, InputId, SessionId, TurnId};

/// The set of a session actor before its first turn: the one every producer
/// that creates the actor with its first wake records. A producer is not
/// the build that serves the session, so it names no state format, and
/// every node that serves sessions decodes it; the owner's `turn.admit`
/// stamps its build's session set (ADR 0106 §1).
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

/// The run session mail's next-turn input opens when the session admits it:
/// the run its source key names, for a host-keyed input, and otherwise the
/// run its own id names. `None` when neither is a run id.
///
/// The one naming rule the session's drain admits by and a turn cancel
/// withdraws by (FIG-5262), so a host addresses a queued input by the run it
/// will open.
#[must_use]
pub fn queued_input_run(input: &InputId, source_key: Option<&str>) -> Option<TurnId> {
    source_key
        .and_then(|key| TurnId::parse(key).ok())
        .or_else(|| TurnId::parse(input.as_str()).ok())
}

impl MailInput {
    /// The run this input opens when the session admits it
    /// ([`queued_input_run`]).
    #[must_use]
    pub fn run(&self) -> Option<TurnId> {
        queued_input_run(&self.input, self.source_key.as_deref())
    }
}

/// What a queued work batch asks the session for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MailBatchKind {
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
    /// The bound rows name `run` as the owner that took them. A row
    /// already bound to `run` stays bound: a running run's phases bind the
    /// steering input and queued turn work its checkpoints delivered with
    /// each phase that records the delivery.
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
    /// Mail the session one next-turn input, open and unbound at the
    /// session's next ingress position, as any producer's acceptance writes
    /// it. A frame switch's `turn.commit` mails its follow-on task this way
    /// (ADR 0101 §3), so the switch and its follow-on commit together. An
    /// input its source key already names is accepted again when it is the
    /// same submission; anything the session refuses, another submission
    /// under the key among it, refuses the commit with
    /// [`DomainRefusal::SessionMailRefused`](super::DomainRefusal::SessionMailRefused).
    Enqueue {
        /// The session.
        session: SessionId,
        /// The input's draft, encoded by its owner.
        draft_json: String,
    },
}
