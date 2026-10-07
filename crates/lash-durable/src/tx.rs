//! The two write authorities: the owner's [`ActorTx`] and everyone else's
//! [`MailTx`].
//!
//! Both are buffered write sets with no I/O of their own. The store applies
//! one in a single transaction when it is committed, so a transaction is
//! never held open across an `.await` in the caller's code.

use crate::domain::{DomainWrite, MailDomainWrite, TurnCancelRequest};
use crate::formats::FormatSet;
use crate::ids::{ActorKey, DurableInstant, Epoch, MailKind, MailSeq, StateRevision};

/// One mail row as the owner reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mail {
    /// Its position in the mailbox.
    pub seq: MailSeq,
    /// What it is.
    pub kind: MailKind,
    /// Its body, as the writer encoded it.
    pub body: String,
    /// When it was appended.
    pub appended_at: DurableInstant,
}

/// How an owner gives an actor up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// Claimable at once by any node that decodes it: a draining node's
    /// release at a committed phase, or the runner's hand-back of an
    /// abandoned activation's actor (`drain.release`).
    Ready,
    /// Nothing to do until mail arrives.
    Idle,
    /// Blocked until mail arrives or, when set, until `next_due` passes.
    Waiting {
        /// The earliest durable deadline the actor waits on.
        next_due: Option<DurableInstant>,
    },
    /// Set aside for an operator with the park the same commit recorded
    /// ([`ParkEventWrite::Park`](crate::domain::ParkEventWrite::Park)):
    /// only a control wake readies it. A pending cancel mail readies it at
    /// once instead.
    Parked,
    /// Finished: never claimed again, and its mailbox refuses mail.
    Terminal,
}

/// The owner's transaction over one actor.
///
/// [`DurableStore::begin`](crate::DurableStore::begin) opens one after a
/// fenced read; it carries the actor's mailbox position and pending mail as
/// of that read. [`DurableStore::commit`](crate::DurableStore::commit)
/// applies it in one transaction whose first statement re-checks the epoch
/// and bumps the actor's state revision; then its [`DomainWrite`]s in the
/// order recorded, then the acknowledgement, then the release. A refused
/// domain write rolls all of it back.
#[derive(Clone, Debug)]
pub struct ActorTx {
    actor: ActorKey,
    epoch: Epoch,
    revision: StateRevision,
    acked: MailSeq,
    seen: MailSeq,
    mail: Vec<Mail>,
    opened_at: DurableInstant,
    turn_cancel: Option<TurnCancelRequest>,
    domain: Vec<DomainWrite>,
    ack: Option<MailSeq>,
    release: Option<Release>,
    formats: Option<FormatSet>,
}

/// What a fenced read saw of one owned actor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedActor {
    /// The actor.
    pub actor: ActorKey,
    /// The epoch the read was fenced on.
    pub epoch: Epoch,
    /// The actor's state revision.
    pub revision: StateRevision,
    /// The mailbox position the owner last acknowledged.
    pub acked: MailSeq,
    /// The newest mailbox position: every append and every wake takes one.
    pub seen: MailSeq,
    /// The pending mail, oldest first.
    pub mail: Vec<Mail>,
    /// The store's clock at the read: the instant a row the owner records
    /// before its work starts takes, such as a model call's deadline.
    pub at: DurableInstant,
    /// For a session actor, the cancel request its unfinished turn
    /// accepted, read with the actor; `None` for any other actor.
    pub turn_cancel: Option<TurnCancelRequest>,
}

impl ActorTx {
    /// The transaction a store's `begin` hands back after its fenced read.
    /// What is recorded on it is applied only by that store's `commit`,
    /// which fences again.
    #[must_use]
    pub fn opened(read: OpenedActor) -> Self {
        Self {
            actor: read.actor,
            epoch: read.epoch,
            revision: read.revision,
            acked: read.acked,
            seen: read.seen,
            mail: read.mail,
            opened_at: read.at,
            turn_cancel: read.turn_cancel,
            domain: Vec::new(),
            ack: None,
            release: None,
            formats: None,
        }
    }

    /// The actor.
    #[must_use]
    pub fn actor(&self) -> &ActorKey {
        &self.actor
    }

    /// The epoch every write is fenced on.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The actor's state revision as of the fenced read.
    #[must_use]
    pub fn revision(&self) -> StateRevision {
        self.revision
    }

    /// The newest mailbox position the fenced read saw.
    #[must_use]
    pub fn seen(&self) -> MailSeq {
        self.seen
    }

    /// Whether anything was appended or woke the actor since the owner last
    /// acknowledged, as of the fenced read. A wake takes a mailbox position
    /// without a mail row, so this can be true with no [`Self::mail`].
    #[must_use]
    pub fn woken(&self) -> bool {
        self.seen > self.acked
    }

    /// The pending mail as of the fenced read, oldest first.
    #[must_use]
    pub fn mail(&self) -> &[Mail] {
        &self.mail
    }

    /// The store's clock at the fenced read: what an owner records as the
    /// start of work it pins before it begins, with no read of its own.
    #[must_use]
    pub fn opened_at(&self) -> DurableInstant {
        self.opened_at
    }

    /// For a session actor, the cancel request its unfinished turn accepted
    /// as of the fenced read; `None` for any other actor.
    #[must_use]
    pub fn turn_cancel(&self) -> Option<&TurnCancelRequest> {
        self.turn_cancel.as_ref()
    }

    /// The domain rows this transaction writes, in order.
    #[must_use]
    pub fn domain(&self) -> &[DomainWrite] {
        &self.domain
    }

    /// Write `write` when this transaction commits, after the fence and the
    /// writes recorded before it.
    pub fn write(&mut self, write: DomainWrite) -> &mut Self {
        self.domain.push(write);
        self
    }

    /// Take the domain rows out of this transaction, for a decorator that
    /// applies them itself in the same database transaction.
    pub fn take_domain(&mut self) -> Vec<DomainWrite> {
        std::mem::take(&mut self.domain)
    }

    /// The position this transaction acknowledges through, if any.
    #[must_use]
    pub fn ack(&self) -> Option<MailSeq> {
        self.ack
    }

    /// How this transaction releases the actor, if it does.
    #[must_use]
    pub fn release(&self) -> Option<Release> {
        self.release
    }

    /// The format set this transaction records the actor's state in, if it
    /// moves it.
    #[must_use]
    pub fn formats(&self) -> Option<&FormatSet> {
        self.formats.as_ref()
    }

    /// Record that the actor's state is written in `formats` from this
    /// commit on: from then, only a node that decodes `formats` claims it.
    pub fn stamp_formats(&mut self, formats: FormatSet) -> &mut Self {
        self.formats = Some(formats);
        self
    }

    /// Acknowledge everything the fenced read saw: its mail and its wakes.
    pub fn ack_seen(&mut self) -> &mut Self {
        self.ack_through(self.seen)
    }

    /// Acknowledge through `through`, deleting the mail up to it.
    /// Acknowledging past [`Self::seen`] is refused at commit with
    /// [`DurableError::AckBeyondRead`](crate::DurableError::AckBeyondRead).
    pub fn ack_through(&mut self, through: MailSeq) -> &mut Self {
        self.ack = Some(self.ack.map_or(through, |ack| ack.max(through)));
        self
    }

    /// Give the actor up when this transaction commits. Mail that is still
    /// unacknowledged then makes it ready at once, unless it ends
    /// [`Release::Terminal`]; the last call wins.
    pub fn give_up(&mut self, release: Release) -> &mut Self {
        self.release = Some(release);
        self
    }
}

/// One mailbox write, as the store applies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailWrite {
    /// Create an actor, ready to be claimed.
    CreateActor {
        /// Its key.
        actor: ActorKey,
        /// The format set its state is written in.
        formats: FormatSet,
    },
    /// Append mail and wake the actor.
    Append {
        /// The addressee.
        actor: ActorKey,
        /// What the mail is.
        kind: MailKind,
        /// Its encoded body.
        body: String,
    },
    /// Wake the actor without mail: something it waits on changed in a
    /// table of its own.
    Wake {
        /// The actor.
        actor: ActorKey,
    },
    /// A conditional domain write; answered in
    /// [`MailCommit::answers`](crate::MailCommit::answers).
    Domain(MailDomainWrite),
}

/// A non-owner's transaction: it creates actors, appends mail and wakes,
/// and nothing else. It has no owner-state writer.
#[derive(Clone, Debug, Default)]
pub struct MailTx {
    writes: Vec<MailWrite>,
}

impl MailTx {
    /// An empty mailbox transaction.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The writes recorded so far, in order.
    #[must_use]
    pub fn writes(&self) -> &[MailWrite] {
        &self.writes
    }

    /// Create `actor`, ready to be claimed. Refused at commit if it exists.
    pub fn create_actor(&mut self, actor: ActorKey, formats: FormatSet) -> &mut Self {
        self.writes.push(MailWrite::CreateActor { actor, formats });
        self
    }

    /// Append mail to `actor` and wake it.
    pub fn append(
        &mut self,
        actor: ActorKey,
        kind: MailKind,
        body: impl Into<String>,
    ) -> &mut Self {
        self.writes.push(MailWrite::Append {
            actor,
            kind,
            body: body.into(),
        });
        self
    }

    /// Wake `actor` without mail.
    pub fn wake(&mut self, actor: ActorKey) -> &mut Self {
        self.writes.push(MailWrite::Wake { actor });
        self
    }

    /// Make the conditional domain write `write` in its place among this
    /// transaction's writes.
    pub fn write(&mut self, write: MailDomainWrite) -> &mut Self {
        self.writes.push(MailWrite::Domain(write));
        self
    }
}
