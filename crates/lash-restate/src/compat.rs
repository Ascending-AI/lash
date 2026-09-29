//! The Restate compatibility contract (ADR 0115 §3.1–3.2).
//!
//! During a roll one invocation can reach a handler another build serves, and
//! object state is shared across deployments. Two shapes protect that, and
//! both are frozen forever:
//!
//! - every cross-build request travels in a [`Call`] that states every wire
//!   version the caller reads, and every reply in a [`Reply`] at the version
//!   the handler selected from it;
//! - every Lash object carries an [`ObjectCompat`] record under
//!   [`COMPAT_KEY`], which names the oldest family formats a build must
//!   support to read or to mutate the object.

pub use lash_sansio::VersionRange;

/// The one wire version of every Lash handler a build other than the
/// caller's can serve.
#[cfg(not(feature = "synthetic-next"))]
pub const RESTATE_WIRE_VERSION: u32 = 1;

/// The wire versions this build reads and answers.
#[cfg(not(feature = "synthetic-next"))]
pub const RESTATE_WIRE: VersionRange = VersionRange::exactly(RESTATE_WIRE_VERSION);

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the wire to 2 and keeps
/// answering N's version 1, so a call from either build selects 1.
#[cfg(feature = "synthetic-next")]
pub const RESTATE_WIRE_VERSION: u32 = 2;

/// The synthetic N+1 reads and answers N's wire and its own.
#[cfg(feature = "synthetic-next")]
pub const RESTATE_WIRE: VersionRange = VersionRange::between(1, RESTATE_WIRE_VERSION);

/// Every cross-build request. JSON `{"wire":{"min":1,"max":1},"body":…}`;
/// the outer shape is frozen.
///
/// The body is encoded at the caller's selected wire version, and `wire`
/// states every version the caller reads, so any build in the compatibility
/// window can read the request and answer it in a shape the caller reads.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Call<T> {
    pub wire: VersionRange,
    pub body: T,
}

impl<T> Call<T> {
    /// A request from this build through ingress: it reads every version of
    /// [`RESTATE_WIRE`]. An ingress request is in no caller's journal.
    pub fn new(body: T) -> Self {
        Self {
            wire: RESTATE_WIRE,
            body,
        }
    }

    /// A request a lash handler sends from its journal.
    ///
    /// The call's bytes are journaled, and a shared handler's journal is
    /// replayed by whichever build resumes it (ADR 0115 §3.5), so they
    /// cannot depend on the build: it states only the wire version the
    /// deployment's fleet epoch selects ([`DeploymentWire::journaled`]).
    pub(crate) fn journaled(body: T) -> Self {
        Self {
            wire: DeploymentWire::current().journaled(),
            body,
        }
    }

    /// The version a handler of this build answers the call at: the highest
    /// both sides read. `None` is the handler's terminal refusal, before it
    /// reads or writes any state.
    pub fn select(&self) -> Option<u32> {
        RESTATE_WIRE.select(self.wire)
    }
}

/// The wire a deployment's lash handlers speak: every version it reads, and
/// the one version its fleet epoch selects for what it writes (ADR 0115
/// §3.1, §3.3). Every build of a compatibility window selects the same
/// version under one `F`: N+1 writes N's until finalize.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeploymentWire {
    pub(crate) reads: VersionRange,
    pub(crate) writes: u32,
}

tokio::task_local! {
    /// The wire of the deployment whose handler is running.
    static DEPLOYMENT_WIRE: DeploymentWire;
}

impl DeploymentWire {
    /// A deployment reading `reads` under `fleet`.
    pub(crate) fn speaking(reads: VersionRange, fleet: lash_core::FleetFormat) -> Self {
        let writes = fleet
            .writer_version(lash_core::surface_format!(RESTATE_WIRE_VERSION))
            .clamp(reads.min(), reads.max());
        Self { reads, writes }
    }

    /// The wire of the deployment whose handler runs this task. A host's
    /// own handler that runs lash code runs under no deployment of lash's,
    /// and speaks this build's wire under its own `F`.
    pub(crate) fn current() -> Self {
        DEPLOYMENT_WIRE
            .try_with(|wire| *wire)
            .unwrap_or_else(|_| Self::speaking(RESTATE_WIRE, lash_core::FleetFormat::current()))
    }

    /// Run `handler` under this wire.
    pub(crate) async fn scope<F: std::future::Future>(self, handler: F) -> F::Output {
        DEPLOYMENT_WIRE.scope(self, handler).await
    }

    /// The range a journaled call states: only the version its fleet epoch
    /// selects, which every build of the window selects alike. Its reply
    /// comes back at that version, which the caller reads because it writes
    /// it. The caller's full range stays out of the journal: a build reading
    /// a wider range replays the same bytes.
    pub(crate) fn journaled(self) -> VersionRange {
        VersionRange::exactly(self.writes)
    }
}

/// Every cross-build reply. JSON `{"wire":1,"body":…}`; frozen.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reply<T> {
    pub wire: u32,
    pub body: T,
}

impl<T> Reply<T> {
    /// A reply at the wire version the handler selected for the call.
    pub fn at(wire: u32, body: T) -> Self {
        Self { wire, body }
    }
}

/// The state key every Lash object keeps its [`ObjectCompat`] record under.
pub const COMPAT_KEY: &str = "_compat";

/// An object's `_compat` record. JSON
/// `{"format":1,"min_reader":1,"min_writer":1}`; frozen and never enveloped.
///
/// Every handler reads it first, after the wire selection and before any
/// other state. Clearing or retiring an object keeps it, and only the next
/// release's `upgrade` handler raises it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ObjectCompat {
    /// The oldest family format any value in the object may carry.
    pub format: u32,
    /// The oldest family format a build must read to read the object.
    pub min_reader: u32,
    /// The oldest family format a build must write to mutate the object.
    pub min_writer: u32,
}

impl ObjectCompat {
    /// The record the first exclusive handler writes on an object with no
    /// other keys, at the family format it selected.
    pub const fn fresh(format: u32) -> Self {
        Self {
            format,
            min_reader: format,
            min_writer: format,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{COMPAT_KEY, Call, ObjectCompat, RESTATE_WIRE, Reply, VersionRange};

    #[test]
    fn restate_call_and_reply_json_is_frozen() {
        let call = Call::new(serde_json::json!({"session_id": "s"}));
        let json = serde_json::to_string(&call).expect("encode");
        assert_eq!(
            json,
            r#"{"wire":{"min":1,"max":1},"body":{"session_id":"s"}}"#
        );
        assert_eq!(
            serde_json::from_str::<Call<serde_json::Value>>(&json).expect("decode"),
            call
        );

        let reply = Reply::at(1, serde_json::json!({"outcome": "done"}));
        let json = serde_json::to_string(&reply).expect("encode");
        assert_eq!(json, r#"{"wire":1,"body":{"outcome":"done"}}"#);
        assert_eq!(
            serde_json::from_str::<Reply<serde_json::Value>>(&json).expect("decode"),
            reply
        );

        let compat = ObjectCompat::fresh(1);
        assert_eq!(COMPAT_KEY, "_compat");
        assert_eq!(
            serde_json::to_string(&compat).expect("encode"),
            r#"{"format":1,"min_reader":1,"min_writer":1}"#
        );
    }

    /// RT0016 (FIG-3805): a journaled call's bytes are the same whatever
    /// range the calling build reads, so a shared journal replays on another
    /// build of the window. N+1 states N's version until finalize moves `F`.
    #[test]
    fn a_journaled_call_states_the_fleet_selected_wire_whatever_the_build_reads() {
        let n_epoch = lash_core::FleetFormat::from_version(1);
        let n = super::DeploymentWire::speaking(VersionRange::exactly(1), n_epoch);
        let wider =
            super::DeploymentWire::speaking(VersionRange::new(1, 2).expect("range"), n_epoch);
        assert_eq!(n.journaled(), VersionRange::exactly(1));
        assert_eq!(wider.journaled(), n.journaled());
        assert_eq!(
            super::DeploymentWire::speaking(RESTATE_WIRE, n_epoch).journaled(),
            VersionRange::exactly(1),
            "this build states N's wire while F is N's epoch"
        );
    }

    #[test]
    fn a_call_selects_the_highest_common_wire() {
        assert_eq!(Call::new(()).select(), Some(1));
        let newer = Call {
            wire: VersionRange::new(1, 2).expect("range"),
            body: (),
        };
        assert_eq!(newer.select(), Some(RESTATE_WIRE.max()));
        let disjoint = Call {
            wire: VersionRange::new(2, 3).expect("range"),
            body: (),
        };
        assert_eq!(disjoint.select(), None);
        assert!(
            serde_json::from_str::<Call<()>>(r#"{"wire":{"min":0,"max":1},"body":null}"#).is_err()
        );
    }
}
