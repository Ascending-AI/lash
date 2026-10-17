#[cfg(test)]
use crate as lash;

// A persistent session's sends decode what they add, not what came before
// (FIG-5930).
//
// A send reads its session's window several times: the engine's turn, the
// runtime its pass opens, the commit's publication, the open session's
// refresh and the handle's terminal report. The store reuses the window it
// decoded and reads only the rows past its leaf, so the node bodies a send
// decodes are bounded by the nodes it appends, whatever the session's
// history depth.

use lash::SessionId;
use lash_core::store::{SessionHistoryStore, WindowSelector};

/// The window reads one send makes: the engine's turn, the runtime its pass
/// opens, the commit's publication, the open session's refresh and the
/// handle's terminal report.
pub const WINDOW_READS_PER_SEND: u64 = 5;
/// Sends before the measured ones: the session's first turns open its
/// runtime and decode the window its creation wrote.
pub const WARM_SENDS: usize = 3;
pub const MEASURED_SENDS: usize = 24;

pub async fn window_len(store: &lash_sqlite_store::SqliteStore, session: &SessionId) -> u64 {
    store
        .load_session_window(session, WindowSelector::Current)
        .await
        .expect("read the window")
        .expect("the session has a window")
        .window
        .nodes
        .len() as u64
}
