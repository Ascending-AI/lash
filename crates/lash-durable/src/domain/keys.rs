//! The keys domain rows are filed under.

use std::fmt;

use crate::ids::ActorKey;
use lash_sansio::{ProcessId, SessionId, TurnId};

/// A code cell's identity within its turn.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CellId(String);

impl CellId {
    /// Name a cell.
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

impl fmt::Display for CellId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One Run (a round of admitted executions) within its owner, from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunSeq(pub u64);

/// One record's ordinal within its Run, from 0. With the owner and the run
/// it is the second fence: one record per ordinal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ordinal(pub u64);

/// Who owns a set of run records: a turn, a process, or a code cell.
/// Stored as `t/<session>/<run>`, `p/<process>` or
/// `c/<session>/<run>/<cell>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OwnerKey {
    /// A turn's tool rounds.
    Turn(SessionId, TurnId),
    /// A process's steps.
    Process(ProcessId),
    /// A code cell's VM-issued operations.
    Cell(SessionId, TurnId, CellId),
}

/// A VM execution with a snapshot: a code cell or a lash_vm process.
/// Stored as `c/<session>/<run>/<cell>` or `p/<process>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExecKey {
    /// A code cell.
    Cell(SessionId, TurnId, CellId),
    /// A lash_vm process.
    Process(ProcessId),
}

/// A scope that owns waits and `Until` children: what a terminal, a turn
/// commit or a session close ends. Stored as `t/<session>/<run>`,
/// `s/<session>`, `p/<process>` or `c/<session>/<run>/<cell>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ScopeKey {
    /// A turn.
    Turn(SessionId, TurnId),
    /// A session.
    Session(SessionId),
    /// A process.
    Process(ProcessId),
    /// A code cell.
    Cell(SessionId, TurnId, CellId),
}

/// A stored key that does not decode.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a stored domain key")]
pub struct StoredKeyError(pub String);

/// Parts are joined by `/`; a part's own `%` and `/` are escaped as `%25`
/// and `%2F`, so any id round-trips.
fn stored(prefix: &str, parts: &[&str]) -> String {
    let mut key = String::from(prefix);
    for part in parts {
        key.push('/');
        key.push_str(&part.replace('%', "%25").replace('/', "%2F"));
    }
    key
}

fn unescape(part: &str) -> String {
    let mut out = String::with_capacity(part.len());
    let mut rest = part;
    while let Some(at) = rest.find('%') {
        out.push_str(&rest[..at]);
        let escape = &rest[at..];
        if let Some(tail) = escape.strip_prefix("%2F") {
            out.push('/');
            rest = tail;
        } else if let Some(tail) = escape.strip_prefix("%25") {
            out.push('%');
            rest = tail;
        } else {
            out.push('%');
            rest = &escape[1..];
        }
    }
    out.push_str(rest);
    out
}

fn split(stored: &str) -> (&str, Vec<String>) {
    let mut parts = stored.split('/');
    let prefix = parts.next().unwrap_or_default();
    (prefix, parts.map(unescape).collect())
}

fn session(id: &str, stored: &str) -> Result<SessionId, StoredKeyError> {
    SessionId::try_from(id.to_owned()).map_err(|_| StoredKeyError(stored.to_owned()))
}

fn turn(id: &str, stored: &str) -> Result<TurnId, StoredKeyError> {
    TurnId::try_from(id.to_owned()).map_err(|_| StoredKeyError(stored.to_owned()))
}

fn process(id: &str, stored: &str) -> Result<ProcessId, StoredKeyError> {
    ProcessId::parse(id).map_err(|_| StoredKeyError(stored.to_owned()))
}

impl OwnerKey {
    /// The stored spelling.
    #[must_use]
    pub fn stored(&self) -> String {
        match self {
            Self::Turn(session, run) => stored("t", &[session.as_str(), run.as_str()]),
            Self::Process(process) => stored("p", &[process.as_str()]),
            Self::Cell(session, run, cell) => {
                stored("c", &[session.as_str(), run.as_str(), cell.as_str()])
            }
        }
    }

    /// A stored spelling read back.
    ///
    /// # Errors
    ///
    /// [`StoredKeyError`] when `stored` is not a key [`Self::stored`] makes.
    pub fn parse(key: &str) -> Result<Self, StoredKeyError> {
        match split(key) {
            ("t", parts) if parts.len() == 2 => {
                Ok(Self::Turn(session(&parts[0], key)?, turn(&parts[1], key)?))
            }
            ("p", parts) if parts.len() == 1 => Ok(Self::Process(process(&parts[0], key)?)),
            ("c", parts) if parts.len() == 3 => Ok(Self::Cell(
                session(&parts[0], key)?,
                turn(&parts[1], key)?,
                CellId::new(parts[2].clone()),
            )),
            _ => Err(StoredKeyError(key.to_owned())),
        }
    }

    /// The actor whose owner writes these records: the turn's or cell's
    /// session, or the process.
    ///
    /// # Panics
    ///
    /// Never: session and process ids are non-empty by construction.
    #[expect(
        clippy::expect_used,
        reason = "session and process ids are non-empty by construction"
    )]
    #[must_use]
    pub fn actor(&self) -> ActorKey {
        match self {
            Self::Turn(session, _) | Self::Cell(session, _, _) => {
                ActorKey::session(session.as_str()).expect("a session id is never empty")
            }
            Self::Process(process) => {
                ActorKey::process(process.as_str()).expect("a process id is never empty")
            }
        }
    }
}

impl ExecKey {
    /// The stored spelling.
    #[must_use]
    pub fn stored(&self) -> String {
        match self {
            Self::Cell(session, run, cell) => {
                stored("c", &[session.as_str(), run.as_str(), cell.as_str()])
            }
            Self::Process(process) => stored("p", &[process.as_str()]),
        }
    }

    /// A stored spelling read back.
    ///
    /// # Errors
    ///
    /// [`StoredKeyError`] when `stored` is not a key [`Self::stored`] makes.
    pub fn parse(key: &str) -> Result<Self, StoredKeyError> {
        match split(key) {
            ("c", parts) if parts.len() == 3 => Ok(Self::Cell(
                session(&parts[0], key)?,
                turn(&parts[1], key)?,
                CellId::new(parts[2].clone()),
            )),
            ("p", parts) if parts.len() == 1 => Ok(Self::Process(process(&parts[0], key)?)),
            _ => Err(StoredKeyError(key.to_owned())),
        }
    }

    /// The run records the execution's admitted operations are filed under.
    #[must_use]
    pub fn owner(&self) -> OwnerKey {
        match self {
            Self::Cell(session, run, cell) => {
                OwnerKey::Cell(session.clone(), run.clone(), cell.clone())
            }
            Self::Process(process) => OwnerKey::Process(process.clone()),
        }
    }
}

impl ScopeKey {
    /// The stored spelling.
    #[must_use]
    pub fn stored(&self) -> String {
        match self {
            Self::Turn(session, run) => stored("t", &[session.as_str(), run.as_str()]),
            Self::Session(session) => stored("s", &[session.as_str()]),
            Self::Process(process) => stored("p", &[process.as_str()]),
            Self::Cell(session, run, cell) => {
                stored("c", &[session.as_str(), run.as_str(), cell.as_str()])
            }
        }
    }

    /// A stored spelling read back.
    ///
    /// # Errors
    ///
    /// [`StoredKeyError`] when `stored` is not a key [`Self::stored`] makes.
    pub fn parse(key: &str) -> Result<Self, StoredKeyError> {
        match split(key) {
            ("t", parts) if parts.len() == 2 => {
                Ok(Self::Turn(session(&parts[0], key)?, turn(&parts[1], key)?))
            }
            ("s", parts) if parts.len() == 1 => Ok(Self::Session(session(&parts[0], key)?)),
            ("p", parts) if parts.len() == 1 => Ok(Self::Process(process(&parts[0], key)?)),
            ("c", parts) if parts.len() == 3 => Ok(Self::Cell(
                session(&parts[0], key)?,
                turn(&parts[1], key)?,
                CellId::new(parts[2].clone()),
            )),
            _ => Err(StoredKeyError(key.to_owned())),
        }
    }
}

macro_rules! display_stored {
    ($($key:ty),*) => {$(
        impl fmt::Display for $key {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.stored())
            }
        }
    )*};
}

display_stored!(OwnerKey, ExecKey, ScopeKey);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_ids_holding_separators() {
        let session = SessionId::try_from("a/b%2Fc%".to_owned()).unwrap();
        let run = TurnId::try_from("r%25/".to_owned()).unwrap();
        for key in [
            OwnerKey::Turn(session.clone(), run.clone()),
            OwnerKey::Cell(session.clone(), run.clone(), CellId::new("x/%")),
        ] {
            assert_eq!(OwnerKey::parse(&key.stored()), Ok(key));
        }
        let scope = ScopeKey::Session(session);
        assert_eq!(ScopeKey::parse(&scope.stored()), Ok(scope));
    }
}
