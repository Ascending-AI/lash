use super::*;
use lash_core_execution::store::WindowSelector;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

/// SQLITE-STATEMENT-REUSE-BOUND: session ids are binds, so warm head/history
/// reads do not compile again for another session; each connection retains at
/// most its configured capacity, even under more distinct shapes than fit.
#[tokio::test]
async fn head_and_history_reuse_statements_with_a_bounded_cache_across_sessions() {
    const CAPACITY: usize = 32;
    const SESSIONS: usize = CAPACITY * 4;
    let mut options = SqliteStoreSetOptions::memory();
    options
        .store
        .connection_policy
        .operational
        .statement_cache_capacity = CAPACITY;
    let stores = SqliteStoreSet::memory_with_options_and_clock(
        options,
        Arc::new(lash_core_execution::facade_support::SystemClock),
    )
    .await
    .expect("stores");
    let store = stores.session_store_factory();
    let sessions: Vec<_> = (0..SESSIONS)
        .map(|index| {
            format!("prepared-session-{index}")
                .parse::<SessionId>()
                .expect("session id")
        })
        .collect();
    for session in &sessions {
        durable_state(&store, session).await;
    }
    let connections: Vec<_> = std::iter::once(store.conn.clone())
        .chain(store.readers.iter().cloned())
        .collect();
    let preparations = Arc::new(AtomicUsize::new(0));
    for connection in &connections {
        let preparations = Arc::clone(&preparations);
        connection
            .call(move |conn| {
                // Installing an authorizer expires existing statements. Install
                // it before warming; SQLite calls Select during compilation only.
                conn.authorizer(Some(move |context: AuthContext<'_>| {
                    if matches!(context.action, AuthAction::Select) {
                        preparations.fetch_add(1, Ordering::Relaxed);
                    }
                    Authorization::Allow
                }));
                Ok(())
            })
            .await
            .expect("authorizer");
    }
    for _ in 0..2 {
        for session in &sessions {
            assert!(
                store
                    .load_session_head_meta(session)
                    .await
                    .unwrap()
                    .is_some()
            );
            assert!(
                store
                    .load_session_window(session, WindowSelector::Current)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
    }
    let warmed = preparations.load(Ordering::Relaxed);
    assert!(
        warmed > 0,
        "the compilation witness must observe cold reads"
    );
    for session in &sessions {
        assert!(
            store
                .load_session_head_meta(session)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .load_session_window(session, WindowSelector::Current)
                .await
                .unwrap()
                .is_some()
        );
    }
    assert_eq!(
        preparations.load(Ordering::Relaxed) - warmed,
        0,
        "warm head/history SQL must be reused across distinct bound sessions"
    );
    for connection in connections {
        connection
            .call(|conn| {
                for index in 0..SESSIONS {
                    let sql = format!("SELECT ?1 AS cache_pressure_{index}");
                    let value: usize = conn
                        .prepare_cached(&sql)?
                        .query_row([index], |row| row.get(0))?;
                    assert_eq!(value, index);
                    assert!(
                        retained_statements(conn) <= CAPACITY,
                        "connection cache exceeded configured capacity"
                    );
                }
                assert_eq!(
                    retained_statements(conn),
                    CAPACITY,
                    "the bound was exercised past eviction"
                );
                Ok(())
            })
            .await
            .expect("bounded connection cache");
    }
}

#[expect(
    unsafe_code,
    reason = "enumerates borrowed SQLite statements on their owning connection thread"
)]
fn retained_statements(connection: &Connection) -> usize {
    let mut count = 0;
    unsafe {
        let mut statement =
            rusqlite::ffi::sqlite3_next_stmt(connection.handle(), std::ptr::null_mut());
        while !statement.is_null() {
            count += 1;
            statement = rusqlite::ffi::sqlite3_next_stmt(connection.handle(), statement);
        }
    }
    count
}
