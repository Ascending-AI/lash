lash_conformance::effect_controller_lease_fencing_tests!({
    let dirs = Arc::new(Mutex::new(Vec::new()));
    let path = fresh_db_path(&dirs, "effect-lease-fencing.db");
    let make_path = path.clone();
    let steal_path = path.clone();
    let expire_path = path.clone();
    let fail_path = path.clone();
    let stall_path = path.clone();
    let heal_path = path.clone();
    // A stall is a second connection holding the database write lock, so the
    // controller's renewal waits in SQLite's busy handler until it is lifted.
    type WriteLockHolder = (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>);
    let write_lock: Arc<Mutex<Option<WriteLockHolder>>> = Arc::new(Mutex::new(None));
    let stall_lock = Arc::clone(&write_lock);
    let heal_lock = Arc::clone(&write_lock);
    (
        dirs,
        lash_conformance::EffectLeaseFencingBackend {
            make_controller: Box::new(move |ttl, clock| {
                let path = make_path.clone();
                Box::pin(async move {
                    let controller = SqliteRuntimeEffectController::open_with_options_and_clock(
                        &path,
                        durable_turn_scope("session", "turn"),
                        SqliteEffectReplayOptions {
                            lease_timings: lash_core_execution::facade_support::LeaseTimings::from_ttl(ttl)
                                .expect("conformance lease timings"),
                            drain_budget: Default::default(),
                        },
                        clock,
                    )
                    .await
                    .expect("controller");
                    let for_replay = controller.clone();
                    lash_conformance::LeaseFencingController {
                        controller: Arc::new(controller),
                        start_replay: Box::new(move || for_replay.start_replay()),
                    }
                })
            }),
            steal_lease: Box::new(move |replay_key| {
                let path = steal_path.clone();
                Box::pin(async move {
                    let stolen_until = current_epoch_ms_for_test().saturating_add(10_000);
                    let conn = rusqlite::Connection::open(&path).expect("open sqlite");
                    let changed = conn
                        .execute(
                            "UPDATE runtime_effect_replay
                             SET lease_owner_id = 'stolen-owner',
                                 lease_token = 'stolen-token',
                                 lease_expires_at_ms = ?1
                             WHERE replay_key = ?2",
                            rusqlite::params![stolen_until as i64, replay_key],
                        )
                        .expect("steal lease row");
                    assert_eq!(changed, 1);
                })
            }),
            expire_lease: Box::new(move |replay_key| {
                let path = expire_path.clone();
                Box::pin(async move {
                    let conn = rusqlite::Connection::open(&path).expect("open sqlite");
                    let changed = conn
                        .execute(
                            "UPDATE runtime_effect_replay
                             SET lease_expires_at_ms = 0
                             WHERE replay_key = ?1",
                            rusqlite::params![replay_key],
                        )
                        .expect("expire lease row");
                    assert_eq!(changed, 1);
                })
            }),
            // A renewal is the only update that keeps the row `in_progress`
            // under the same lease token while moving its expiry; a takeover
            // changes the token and a finalize changes the status.
            fail_renewals: Box::new(move |replay_key| {
                let path = fail_path.clone();
                Box::pin(async move {
                    let conn = rusqlite::Connection::open(&path).expect("open sqlite");
                    conn.execute_batch(&format!(
                        "CREATE TRIGGER lash_conformance_fail_effect_renewal
                         BEFORE UPDATE OF lease_expires_at_ms ON runtime_effect_replay
                         WHEN OLD.replay_key = '{replay_key}'
                          AND NEW.status = 'in_progress'
                          AND OLD.lease_token IS NEW.lease_token
                         BEGIN
                           SELECT RAISE(ABORT, 'injected effect lease renewal fault');
                         END;"
                    ))
                    .expect("install renewal fault");
                })
            }),
            stall_renewals: Box::new(move |_replay_key| {
                let path = stall_path.clone();
                let write_lock = Arc::clone(&stall_lock);
                Box::pin(async move {
                    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
                    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
                    let holder = std::thread::spawn(move || {
                        let conn = rusqlite::Connection::open(&path).expect("open sqlite");
                        conn.execute_batch("BEGIN IMMEDIATE")
                            .expect("hold the write lock");
                        let _ = locked_tx.send(());
                        let _ = release_rx.recv_timeout(std::time::Duration::from_secs(60));
                        conn.execute_batch("ROLLBACK")
                            .expect("release the write lock");
                    });
                    locked_rx.await.expect("write lock held");
                    *write_lock.lock_recover() = Some((release_tx, holder));
                })
            }),
            heal_renewals: Box::new(move |_replay_key| {
                let path = heal_path.clone();
                let write_lock = Arc::clone(&heal_lock);
                Box::pin(async move {
                    let holder = write_lock.lock_recover().take();
                    if let Some((release, holder)) = holder {
                        let _ = release.send(());
                        tokio::task::spawn_blocking(move || holder.join())
                            .await
                            .expect("join the write-lock holder")
                            .expect("write-lock holder exits cleanly");
                    }
                    let conn = rusqlite::Connection::open(&path).expect("open sqlite");
                    conn.execute_batch(
                        "DROP TRIGGER IF EXISTS lash_conformance_fail_effect_renewal;",
                    )
                    .expect("remove renewal fault");
                })
            }),
        },
    )
});
