//! Opt-in first-use markers sharing one monotonic epoch in this process.
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static EPOCH: OnceLock<Instant> = OnceLock::new();
static MARKERS: Mutex<Option<Vec<Marker>>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    ProcessEntry,
    StoreOpenStarted,
    StoreOpened,
    StoreSetupFinished,
    StoresReady,
    NodeRegistered,
    CoreBuilt,
    SessionOpened,
    VmSpawnStarted,
    VmSpawned,
    VmReady,
    ProviderFirstRequest,
    FirstResult,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Marker {
    pub phase: Phase,
    /// Nanoseconds since this process's instrumented entry; one point sample.
    pub since_entry_ns: u64,
    /// Observer process, including for parent-observed VM spawn/ready.
    pub observer_pid: u32,
    pub subject_pid: u32,
}

pub fn initialize_epoch() {
    EPOCH.get_or_init(Instant::now);
}

/// One bounded recorder per process. It retains only the first of each phase.
pub struct Recorder;
impl Recorder {
    pub fn install() -> Result<Self, super::AlreadyInstalled> {
        initialize_epoch();
        let mut markers = MARKERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if markers.is_some() {
            return Err(super::AlreadyInstalled);
        }
        *markers = Some(vec![Marker {
            phase: Phase::ProcessEntry,
            since_entry_ns: 0,
            observer_pid: std::process::id(),
            subject_pid: std::process::id(),
        }]);
        Ok(Self)
    }
    pub fn snapshot(&self) -> Vec<Marker> {
        MARKERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()
            .unwrap_or_default()
    }
}
impl Drop for Recorder {
    fn drop(&mut self) {
        *MARKERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}
pub fn record(phase: Phase) {
    record_subject(phase, std::process::id());
}
pub fn record_subject(phase: Phase, subject_pid: u32) {
    let mut markers = MARKERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(markers) = markers.as_mut() else {
        return;
    };
    if markers.iter().any(|marker| marker.phase == phase) {
        return;
    }
    markers.push(Marker {
        phase,
        since_entry_ns: EPOCH
            .get_or_init(Instant::now)
            .elapsed()
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64,
        observer_pid: std::process::id(),
        subject_pid,
    });
}
