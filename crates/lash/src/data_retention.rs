//! The host's data-retention statement ([`DataRetention`]).

use crate::observe::InMemoryLiveReplayStoreConfig;
use crate::persistence::AttachmentPolicy;
use crate::process::InMemoryProcessReplayStoreConfig;
use crate::process::ProcessObservationConfig;

/// What a host keeps, how much of it and for how long. A core is not built
/// without one ([`LashCoreBuilder::data_retention`](crate::LashCoreBuilder::data_retention)):
/// these are the host's decisions, and lash has no default for any of them
/// (D-DEFAULTS2). Every field may be stated as a bound or as its explicit
/// unbounded or keep-everything value.
#[derive(Clone, Debug)]
pub struct DataRetention {
    /// The attachment put bound (`None` states unbounded), the read budgets,
    /// the upload expiry, and which outputs leave history for a retained
    /// attachment.
    pub attachments: AttachmentPolicy,
    /// Which revisions every session this core creates keeps besides its
    /// head and its pins: until the host collects, the last `n` turns, or
    /// the head only. Recorded with the session;
    /// [`LashSession::set_retention`](crate::LashSession::set_retention)
    /// changes one session's afterwards.
    pub session_revisions: crate::Retention,
    /// What the core's built-in live replay buffer keeps for reconnecting
    /// observers. A host that installs its own store
    /// ([`LashCoreBuilder::live_replay_store`](crate::LashCoreBuilder::live_replay_store))
    /// states that store's retention where it constructs it, and this value
    /// then configures nothing.
    pub live_replay: InMemoryLiveReplayStoreConfig,
    /// What the core's built-in process replay buffer keeps for process
    /// observers, apart from `live_replay`: a process's window and a
    /// session's never share a budget. A host that installs its own store
    /// ([`LashCoreBuilder::process_replay_store`](crate::LashCoreBuilder::process_replay_store))
    /// states that store's retention where it constructs it, and this value
    /// then configures nothing.
    pub process_replay: InMemoryProcessReplayStoreConfig,
    /// What the process observation hub keeps for live observers, and what
    /// one snapshot may read.
    pub process_observation: ProcessObservationConfig,
}

impl DataRetention {
    /// The standard retention, which a host names to choose it:
    ///
    /// - attachments: [`AttachmentPolicy::standard`] (unbounded puts, reads
    ///   of 32 MiB per blob and 128 MiB per request, a 24-hour upload
    ///   expiry, outputs over 64 KiB retained behind a 4 KiB witness);
    /// - session revisions: [`Retention::UntilGc`](crate::Retention::UntilGc),
    ///   every revision forkable until the host collects;
    /// - live replay: [`InMemoryLiveReplayStoreConfig::standard`] (2,048
    ///   events per session for 120 seconds, 4,096 sessions, 64 MiB);
    /// - process replay: [`InMemoryProcessReplayStoreConfig::standard`]
    ///   (2,048 events per process for 120 seconds, 8 MiB per process, 4,096
    ///   processes, 64 MiB);
    /// - process observation: [`ProcessObservationConfig::standard`] (a
    ///   2,048-item ring kept 120 idle seconds, snapshots of 64 pages of 256
    ///   events).
    ///
    /// No measurement backs these values. The retained-output limit alone
    /// is sized against something: the standard renderer's 16,000-character
    /// cut stays inline under it.
    pub const fn standard() -> Self {
        Self {
            attachments: AttachmentPolicy::standard(),
            session_revisions: crate::Retention::UntilGc,
            live_replay: InMemoryLiveReplayStoreConfig::standard(),
            process_replay: InMemoryProcessReplayStoreConfig::standard(),
            process_observation: ProcessObservationConfig::standard(),
        }
    }
}
