//! The port's value types: identities, epochs, instants and labels.

use std::fmt;

/// Which kind of actor a key names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ActorKind {
    /// A session: its turns, shifts and session commands.
    Session,
    /// A process: it may outlive its session, so it is its own actor.
    Process,
}

impl ActorKind {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Process => "process",
        }
    }

    /// The stored spelling read back; `None` for anything else.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        match stored {
            "session" => Some(Self::Session),
            "process" => Some(Self::Process),
            _ => None,
        }
    }

    const fn prefix(self) -> &'static str {
        match self {
            Self::Session => "s/",
            Self::Process => "p/",
        }
    }
}

/// An actor's key: `s/<session id>` or `p/<process id>`.
///
/// The kind is part of the key, so a session and a process with the same
/// id are different actors.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActorKey(String);

/// A refused [`ActorKey`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ActorKeyError {
    /// The id part is empty.
    #[error("an actor id must not be empty")]
    EmptyId,
    /// The stored key has no `s/` or `p/` prefix.
    #[error("`{0}` is not an actor key: it needs an `s/` or `p/` prefix")]
    UnknownPrefix(String),
}

impl ActorKey {
    /// The key of the session `id`.
    ///
    /// # Errors
    ///
    /// [`ActorKeyError::EmptyId`] for an empty id.
    pub fn session(id: &str) -> Result<Self, ActorKeyError> {
        Self::of(ActorKind::Session, id)
    }

    /// The key of the process `id`.
    ///
    /// # Errors
    ///
    /// [`ActorKeyError::EmptyId`] for an empty id.
    pub fn process(id: &str) -> Result<Self, ActorKeyError> {
        Self::of(ActorKind::Process, id)
    }

    /// The key of the `kind` actor `id`.
    ///
    /// # Errors
    ///
    /// [`ActorKeyError::EmptyId`] for an empty id.
    pub fn of(kind: ActorKind, id: &str) -> Result<Self, ActorKeyError> {
        if id.is_empty() {
            return Err(ActorKeyError::EmptyId);
        }
        Ok(Self(format!("{}{id}", kind.prefix())))
    }

    /// A stored key read back.
    ///
    /// # Errors
    ///
    /// [`ActorKeyError`] when `stored` is not a key [`Self::of`] would make.
    pub fn parse(stored: &str) -> Result<Self, ActorKeyError> {
        for kind in [ActorKind::Session, ActorKind::Process] {
            if let Some(id) = stored.strip_prefix(kind.prefix()) {
                return Self::of(kind, id);
            }
        }
        Err(ActorKeyError::UnknownPrefix(stored.to_owned()))
    }

    /// The kind this key names.
    #[must_use]
    pub fn kind(&self) -> ActorKind {
        if self.0.starts_with(ActorKind::Process.prefix()) {
            ActorKind::Process
        } else {
            ActorKind::Session
        }
    }

    /// The id without its kind prefix.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.0[2..]
    }

    /// The stored spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ActorKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An actor's ownership epoch. Claim, reap and release each bump it, and
/// nothing else changes it; an owner transaction commits only under the
/// current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Epoch(pub i64);

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// How many owner commits an actor's state has taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StateRevision(pub i64);

/// A mail row's position in its actor's mailbox. Mail is acknowledged
/// through a position, never by gaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MailSeq(pub i64);

/// A durable instant: milliseconds since the Unix epoch on the store's
/// clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DurableInstant(pub i64);

impl DurableInstant {
    /// This instant moved `millis` later, saturating.
    #[must_use]
    pub const fn after_millis(self, millis: i64) -> Self {
        Self(self.0.saturating_add(millis))
    }
}

/// A serving node's stable name, chosen by the host.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(String);

impl NodeId {
    /// Name a node.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The stored spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One process start of a node: fresh per registration, so a restarted
/// node never inherits its predecessor's lease.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BootId(String);

impl BootId {
    /// A boot identity as stored.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The stored spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The digest of a set of durable formats. An actor records the set its
/// state is written in; a node declares the sets it decodes, and claims only
/// actors it can decode.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FormatSet(String);

impl FormatSet {
    /// A format-set digest as stored.
    #[must_use]
    pub fn new(digest: impl Into<String>) -> Self {
        Self(digest.into())
    }

    /// The stored spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a mail row is: its consumer's vocabulary, not the port's.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MailKind(String);

impl MailKind {
    /// A mail kind as stored.
    #[must_use]
    pub fn new(kind: impl Into<String>) -> Self {
        Self(kind.into())
    }

    /// The stored spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The name of one durable commit point, e.g. `turn.admit`.
///
/// Every transaction the port runs carries one, so a harness can cut the
/// run at any commit and name the cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommitLabel(&'static str);

impl CommitLabel {
    /// [`DurableStore::claim`](crate::DurableStore::claim)'s transaction.
    pub const CLAIM: Self = Self("claim");
    /// [`DurableStore::heartbeat`](crate::DurableStore::heartbeat)'s transaction.
    pub const HEARTBEAT: Self = Self("heartbeat");
    /// [`DurableStore::reap`](crate::DurableStore::reap)'s transaction.
    pub const REAP: Self = Self("reap");
    /// [`DurableStore::register_node`](crate::DurableStore::register_node)'s transaction.
    pub const NODE_REGISTER: Self = Self("node.register");
    /// [`DurableStore::release_node`](crate::DurableStore::release_node)'s transaction.
    pub const NODE_RELEASE: Self = Self("node.release");

    /// Name a commit point.
    #[must_use]
    pub const fn new(label: &'static str) -> Self {
        Self(label)
    }

    /// The label's text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for CommitLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
