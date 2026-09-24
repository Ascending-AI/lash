//! The grouped-effect runner machinery of the generated surface
//! differential: opener threads, per-backend group hosts, the release-flag
//! child resolver and the opener command dispatch.

use super::*;

/// Grouped children journal under their own session scope, so nothing else in
/// the generated surface touches their rows.
pub(super) const SURFACE_GROUP_SESSION: &str = "surface-group-session";
pub(super) const SURFACE_GROUP_TURN: &str = "surface-group-turn";
/// The two-concurrent-finals group. Its commit and settlement order is a
/// scheduler fact, so the differential compares that group's arbitration and
/// rank columns order-free (per-row values are nulled and the sorted sets are
/// attached to the group row).
pub(super) const UNORDERED_GROUP: u8 = 2;
pub(super) const UNORDERED_GROUP_KEY: &str = "surface-group-2";
/// Short leases for the dedicated SystemClock group hosts: the crash-recovery
/// scenario needs a dead opener's claims to lapse in test time.
pub(super) const GROUP_LEASE_TTL: Duration = Duration::from_millis(300);
pub(super) const GROUP_LEASE_RENEW: Duration = Duration::from_millis(100);
/// Every group command through an opener, and every poll loop the runner
/// drives, is bounded so a broken substrate hangs nowhere.
pub(super) const GROUP_OP_BOUND: Duration = Duration::from_secs(15);
pub(super) const GROUP_POLL: Duration = Duration::from_millis(25);

/// The group-child resolver every grouped surface runner registers.
///
/// A child parks on its `(group_key, position)` release flag until an
/// `EffectGroupRelease` op flips it, so commit and drain order across
/// backends is decided by the generated sequence rather than by the
/// scheduler. The `started` flag flips when the executor is invoked, which is
/// after the child's claim row is written — the witness a `Commit` op waits
/// on before reaching the §4 boundary. Both flags persist: a drain that
/// re-resolves a released child gets an executor that completes immediately.
pub(super) struct DifferentialGroupExecutors {
    pub(super) flags: Mutex<BTreeMap<(String, usize), Arc<GroupChildFlags>>>,
}

#[derive(Default)]
pub(super) struct GroupChildFlags {
    pub(super) started: AtomicBool,
    pub(super) release: AtomicBool,
}

#[expect(
    clippy::expect_used,
    reason = "test support: a poisoned flag map panics the harness; there is no refusal to record"
)]
impl DifferentialGroupExecutors {
    pub(super) fn flags(&self, group_key: &str, position: usize) -> Arc<GroupChildFlags> {
        self.flags
            .lock()
            .expect("differential group flag map")
            .entry((group_key.to_string(), position))
            .or_default()
            .clone()
    }

    pub(super) fn release(&self, group_key: &str, position: usize) {
        self.flags(group_key, position)
            .release
            .store(true, Ordering::SeqCst);
    }

    pub(super) fn started(&self, group_key: &str, position: usize) -> bool {
        self.flags
            .lock()
            .expect("differential group flag map")
            .get(&(group_key.to_string(), position))
            .is_some_and(|flags| flags.started.load(Ordering::SeqCst))
    }
}

impl Default for DifferentialGroupExecutors {
    fn default() -> Self {
        Self {
            flags: Mutex::new(BTreeMap::new()),
        }
    }
}

impl GroupExecutors for DifferentialGroupExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        let membership = envelope.group.as_ref()?;
        let flags = self.flags(&membership.group_key, membership.position);
        Some(RuntimeEffectLocalExecutor::testing(move |_| {
            let flags = Arc::clone(&flags);
            async move {
                flags.started.store(true, Ordering::SeqCst);
                while !flags.release.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
                Ok(RuntimeEffectOutcome::Sleep)
            }
        }))
    }
}

/// One group's opener process: a thread, its own multi-thread runtime, and a
/// host whose driver owns the opener's lease identity. Crash is real: the
/// runtime is shut down and the child tasks it dispatched die with it, which
/// is the only thing that stops their lease renewals.
pub(super) struct GroupOpener {
    pub(super) commands: Option<tokio::sync::mpsc::UnboundedSender<GroupOpMessage>>,
    pub(super) thread: Option<std::thread::JoinHandle<()>>,
}

pub(super) type GroupOpMessage = (GroupOpCommand, tokio::sync::oneshot::Sender<GroupOpReply>);

pub(super) enum GroupOpCommand {
    Open { group: Box<RuntimeEffectGroup> },
    Await,
    Close { disposition: LoserPolicy },
    Commit { position: u8 },
    CommitBoth { a: u8, b: u8 },
    DrainBlocked { commit_seq: u64 },
}

/// What an opener thread reports back: the JSON the runner records in
/// `group_outcomes`, plus the `(position, commit_seq)` pairs a commit
/// established, so `DrainBlocked` can translate a rank into a sequence.
pub(super) struct GroupOpReply {
    pub(super) outcome: serde_json::Value,
    pub(super) committed: Vec<(u8, u64)>,
}

/// A group host, concretely typed because each store's host carries its own
/// construction options. The controller handed to the opener thread is the
/// host's scoped controller as an `Arc<dyn RuntimeEffectController>`.
pub(super) enum GroupHost {
    Sqlite(SqliteEffectHost),
    Postgres(PostgresEffectHost),
}

impl GroupHost {
    pub(super) fn register_group_executors(
        &self,
        executors: Arc<dyn GroupExecutors>,
    ) -> Result<(), String> {
        match self {
            Self::Sqlite(host) => host.register_group_executors(executors),
            Self::Postgres(host) => host.register_group_executors(executors),
        }
        .map_err(|error| neutral_error_code(&error))
    }

    pub(super) fn group_drain(&self) -> Arc<dyn StoreEffectGroupDrain> {
        match self {
            Self::Sqlite(host) => host.group_drain(),
            Self::Postgres(host) => host.group_drain(),
        }
    }

    /// The backend's §5 barrier read, straight from its row store.
    pub(super) async fn drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        match self {
            Self::Sqlite(host) => host.drain_blocked_for_testing(group_key, commit_seq).await,
            Self::Postgres(host) => host.drain_blocked_for_testing(group_key, commit_seq).await,
        }
    }

    pub(super) fn group_controller(&self) -> Result<Arc<dyn RuntimeEffectController>, String> {
        let admitted = AdmittedScope::unpinned(group_scope()).map_err(|error| error.to_string())?;
        let scoped = match self {
            Self::Sqlite(host) => host.scoped_static(admitted),
            Self::Postgres(host) => host.scoped_static(admitted),
        }
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "effect host offered no static differential controller".to_string())?;
        scoped
            .owned_controller()
            .ok_or_else(|| "scoped group controller is not shared".to_string())
    }
}

/// The grouped surface one runner carries: the shared child resolver, the
/// successor host the drain runs over, and one opener per opened group.
pub(super) struct GroupSurface {
    pub(super) executors: Arc<DifferentialGroupExecutors>,
    pub(super) backend: GroupBackend,
    /// The successor host the `Drain` op runs over — a different lease
    /// identity from the openers, which is what lets it fence them out.
    pub(super) successor: GroupHost,
    pub(super) openers: BTreeMap<u8, GroupOpener>,
}

#[derive(Clone)]
pub(super) enum GroupBackend {
    Sqlite {
        path: PathBuf,
    },
    /// Openers dial their own pool rather than borrowing the shared one: a
    /// crashed opener strand connections whose sockets belong to a dead
    /// runtime, and on a shared pool those dead handles would block every
    /// later acquire. The successor host keeps the shared pool — it is never
    /// crashed.
    Postgres {
        database_url: String,
    },
}

/// Build one opener's host on the opener's own backend resources, then wire
/// the shared child resolver into it.
pub(super) async fn group_opener_host(
    backend: &GroupBackend,
    executors: &Arc<DifferentialGroupExecutors>,
) -> Result<GroupHost, String> {
    let host = match backend {
        GroupBackend::Sqlite { path } => GroupHost::Sqlite(
            SqliteEffectHost::open_with_options_and_clock(
                path,
                sqlite_group_options(),
                Arc::new(SystemClock),
            )
            .await
            .map_err(|error| error.to_string())?,
        ),
        GroupBackend::Postgres { database_url } => {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(4)
                .connect(database_url)
                .await
                .map_err(|error| error.to_string())?;
            let storage = PostgresStorage::from_pool(pool)
                .await
                .map_err(|error| error.to_string())?;
            GroupHost::Postgres(PostgresEffectHost::with_options_and_clock(
                &storage,
                postgres_group_options(),
                Arc::new(SystemClock),
            ))
        }
    };
    host.register_group_executors(Arc::clone(executors) as Arc<dyn GroupExecutors>)?;
    Ok(host)
}

impl Drop for GroupSurface {
    fn drop(&mut self) {
        for opener in self.openers.values_mut() {
            opener.shutdown();
        }
    }
}

impl GroupOpener {
    /// Spawn the opener thread with its own runtime and dispatch loop. The
    /// backend handle is cloned in and the host is built inside the thread's
    /// runtime so the opener's lease identity, its claims and — for Postgres —
    /// its connection pool all belong to the runtime that dies on `crash`.
    /// Setup errors come back over `ready` so `spawn` reports them instead of
    /// stranding the caller on a dead inbox.
    #[expect(
        clippy::expect_used,
        reason = "test support: an opener that cannot build a runtime panics the harness; there is no refusal to record"
    )]
    pub(super) fn spawn(
        backend: GroupBackend,
        executors: Arc<DifferentialGroupExecutors>,
        group_key: String,
        scope_id: String,
    ) -> Result<Self, String> {
        let (commands, mut inbox) = tokio::sync::mpsc::unbounded_channel::<GroupOpMessage>();
        let (ready, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let thread = std::thread::Builder::new()
            .name(format!("{group_key}-opener"))
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("a group opener builds its own runtime");
                runtime.block_on(async move {
                    let setup = async {
                        let host = group_opener_host(&backend, &executors).await?;
                        let controller = host.group_controller()?;
                        Ok::<_, String>(OpenerSide { host, controller })
                    };
                    let opener = match setup.await {
                        Ok(parts) => parts,
                        Err(error) => {
                            let _ = ready.send(Err(error));
                            return;
                        }
                    };
                    let _ = ready.send(Ok(()));
                    let mut handle: Option<EffectGroupHandle> = None;
                    while let Some((command, reply)) = inbox.recv().await {
                        let outcome = dispatch_group_command(
                            &opener,
                            &executors,
                            &group_key,
                            &scope_id,
                            group_key == UNORDERED_GROUP_KEY,
                            &mut handle,
                            command,
                        )
                        .await;
                        let _ = reply.send(outcome);
                    }
                });
                runtime.shutdown_timeout(GROUP_OP_BOUND);
            })
            .map_err(|error| error.to_string())?;
        ready_rx
            .recv()
            .map_err(|_| "group opener thread exited during setup".to_string())??;
        Ok(Self {
            commands: Some(commands),
            thread: Some(thread),
        })
    }
}

pub(super) fn group_scope() -> ExecutionScope {
    ExecutionScope::turn(SURFACE_GROUP_SESSION, SURFACE_GROUP_TURN)
}

/// The `scope_id` string the journal actually stores: the scope's wire
/// journal key, which is what `commit_group_child` and every reader row
/// carry.
#[expect(
    clippy::expect_used,
    reason = "test support: the fixed group scope is constructed admitted; a refusal is a harness defect"
)]
pub(super) fn group_scope_id() -> String {
    group_scope()
        .journal_identity()
        .expect("the surface group scope is a valid journal identity")
        .key()
        .to_string()
}

pub(super) fn group_key(group: u8) -> String {
    format!("surface-group-{group}")
}

pub(super) fn group_child_replay_key(group: u8, position: u8) -> String {
    format!("{}:child:{position}", group_key(group))
}

#[expect(
    clippy::expect_used,
    reason = "test support: the fixed lease timings are validated by the ratio contract; a refusal is a harness defect"
)]
pub(super) fn sqlite_group_options() -> SqliteEffectReplayOptions {
    SqliteEffectReplayOptions {
        lease_timings: LeaseTimings::new(GROUP_LEASE_TTL, GROUP_LEASE_RENEW)
            .expect("group lease timings keep the 3:1 TTL-to-renew ratio"),
        drain_budget: Default::default(),
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the fixed lease timings are validated by the ratio contract; a refusal is a harness defect"
)]
pub(super) fn postgres_group_options() -> PostgresEffectReplayOptions {
    PostgresEffectReplayOptions {
        lease_timings: LeaseTimings::new(GROUP_LEASE_TTL, GROUP_LEASE_RENEW)
            .expect("group lease timings keep the 3:1 TTL-to-renew ratio"),
        drain_budget: Default::default(),
    }
}

/// The group a script opens: `children` `LanguageRuntimeValue` children (any
/// journaled command the driver delegates to the local executor works; `Sleep`
/// is answered by the driver itself and would never park, and `ExecCode` is
/// never journaled so it cannot be a group child), `All` wake, declared
/// disposition.
#[expect(
    clippy::expect_used,
    reason = "test support: generated group shapes are constructed admitted; a refusal is a harness defect"
)]
pub(super) fn surface_group(group: u8, children: u8, cancel_losers: bool) -> RuntimeEffectGroup {
    let scope = group_scope();
    let key = group_key(group);
    let child_envelopes = (0..children)
        .map(|position| {
            let replay_key = group_child_replay_key(group, position);
            RuntimeEffectEnvelope::new(
                lash_core::RuntimeEffectInvocation::new(
                    EffectAddress::new(scope.clone(), replay_key.clone())
                        .expect("a group child carries an admitted effect scope"),
                    RuntimeAttribution::for_turn(SURFACE_GROUP_SESSION, SURFACE_GROUP_TURN, 1, 0),
                    replay_key,
                ),
                RuntimeEffectCommand::LanguageRuntimeValue {
                    operation: format!("surface-group:{key}:{position}"),
                },
            )
        })
        .collect();
    RuntimeEffectGroup::try_new(
        lash_core::RuntimeEffectInvocation::new(
            EffectAddress::new(scope, format!("{key}:open"))
                .expect("a group opener carries an admitted effect scope"),
            RuntimeAttribution::for_turn(SURFACE_GROUP_SESSION, SURFACE_GROUP_TURN, 1, 0),
            format!("{key}:open"),
        ),
        key,
        child_envelopes,
        GroupWakePolicy::All,
        if cancel_losers {
            LoserPolicy::Cancel
        } else {
            LoserPolicy::RunToCompletion
        },
    )
    .expect("generated group shape is well-formed")
}

/// A controller error crosses the comparison as its code alone — message
/// text is backend-local — with the backend namespacing stripped, so
/// `SqliteEffectReplayCorruptRow` and `PostgresEffectReplayCorruptRow` agree.
pub(super) fn neutral_error_code(error: &lash_core::RuntimeEffectControllerError) -> String {
    let code = format!("{:?}", error.code);
    code.strip_prefix("Sqlite")
        .or_else(|| code.strip_prefix("Postgres"))
        .unwrap_or(&code)
        .to_string()
}

/// What an opener thread holds: its host, for the row-store reads a probe
/// makes, and the host's scoped group controller every other command drives.
pub(super) struct OpenerSide {
    host: GroupHost,
    controller: Arc<dyn RuntimeEffectController>,
}

/// Run one opener-bound command on the opener thread. `handle` is the
/// group's open cursor, owned by the opener so the caller cannot touch it.
pub(super) async fn dispatch_group_command(
    opener: &OpenerSide,
    executors: &Arc<DifferentialGroupExecutors>,
    group_key: &str,
    scope_id: &str,
    unordered: bool,
    handle: &mut Option<EffectGroupHandle>,
    command: GroupOpCommand,
) -> GroupOpReply {
    let OpenerSide { host, controller } = opener;
    let none = Vec::new();
    match command {
        GroupOpCommand::Open { group } => match controller.open_effect_group(*group).await {
            Ok(opened) => {
                *handle = Some(opened);
                GroupOpReply {
                    outcome: serde_json::json!({"opened": true}),
                    committed: none,
                }
            }
            Err(error) => GroupOpReply {
                outcome: serde_json::json!({"error": neutral_error_code(&error)}),
                committed: none,
            },
        },
        GroupOpCommand::Await => {
            let Some(handle) = handle.as_mut() else {
                return GroupOpReply {
                    outcome: serde_json::json!({"error": "effect group is not open"}),
                    committed: none,
                };
            };
            let awaited = tokio::time::timeout(
                GROUP_OP_BOUND,
                controller.await_next_settlement(handle, CancellationToken::new()),
            )
            .await;
            let outcome = match awaited {
                Err(_) => serde_json::json!({"error": "await_timeout"}),
                Ok(Err(error)) => serde_json::json!({"error": neutral_error_code(&error)}),
                Ok(Ok(settlement)) => serde_json::json!({
                    "position": if unordered {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!(settlement.position)
                    },
                    "sequence": settlement.sequence,
                    "ok": settlement.outcome.is_ok(),
                }),
            };
            GroupOpReply {
                outcome,
                committed: none,
            }
        }
        GroupOpCommand::Close { disposition } => {
            let Some(handle) = handle.take() else {
                return GroupOpReply {
                    outcome: serde_json::json!({"error": "effect group is not open"}),
                    committed: none,
                };
            };
            let outcome = match controller.close_effect_group(handle, disposition).await {
                Ok(()) => serde_json::json!({"closed": true}),
                Err(error) => serde_json::json!({"error": neutral_error_code(&error)}),
            };
            GroupOpReply {
                outcome,
                committed: none,
            }
        }
        GroupOpCommand::Commit { position } => {
            if !wait_group_child_started(executors, group_key, position).await {
                return GroupOpReply {
                    outcome: serde_json::json!({"error": "child_not_started"}),
                    committed: none,
                };
            }
            let outcome = commit_group_child(controller, scope_id, group_key, position).await;
            let committed = commit_reply_seq(position, &outcome).into_iter().collect();
            GroupOpReply { outcome, committed }
        }
        GroupOpCommand::CommitBoth { a, b } => {
            for position in [a, b] {
                if !wait_group_child_started(executors, group_key, position).await {
                    return GroupOpReply {
                        outcome: serde_json::json!({"error": "child_not_started"}),
                        committed: none,
                    };
                }
            }
            let (first, second) = tokio::join!(
                commit_group_child(controller, scope_id, group_key, a),
                commit_group_child(controller, scope_id, group_key, b),
            );
            // Sorted by sequence: which position won which rank is a
            // scheduler fact, so the recorded outcome carries kinds and
            // sequences only.
            let mut entries: Vec<(String, Option<u64>)> = [&first, &second]
                .into_iter()
                .map(|outcome| (outcome_kind(outcome).to_string(), outcome_seq(outcome)))
                .collect();
            entries.sort();
            let law_ok = entries.len() == 2
                && entries.iter().all(|(kind, _)| kind == "committed")
                && entries[0]
                    .1
                    .zip(entries[1].1)
                    .is_some_and(|(low, high)| low + 1 == high);
            let committed = [(a, &first), (b, &second)]
                .into_iter()
                .filter_map(|(position, outcome)| outcome_seq(outcome).map(|seq| (position, seq)))
                .collect();
            GroupOpReply {
                outcome: serde_json::json!({
                    "commits": entries
                        .iter()
                        .map(|(kind, seq)| serde_json::json!([kind, seq]))
                        .collect::<Vec<_>>(),
                    "law_ok": law_ok,
                }),
                committed,
            }
        }
        GroupOpCommand::DrainBlocked { commit_seq } => {
            // The controller only waits the barrier out, so the probe reads
            // each backend's row-store answer directly.
            let outcome = match host.drain_blocked(group_key, commit_seq).await {
                Ok(blocked) => serde_json::json!({
                    "commit_seq": commit_seq,
                    "blocked": blocked,
                }),
                Err(error) => serde_json::json!({"error": neutral_error_code(&error)}),
            };
            GroupOpReply {
                outcome,
                committed: none,
            }
        }
    }
}

pub(super) fn outcome_kind(outcome: &serde_json::Value) -> &str {
    outcome
        .get("outcome")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("error")
}

pub(super) fn outcome_seq(outcome: &serde_json::Value) -> Option<u64> {
    outcome
        .get("commit_seq")
        .and_then(serde_json::Value::as_u64)
}

pub(super) fn commit_reply_seq(position: u8, outcome: &serde_json::Value) -> Option<(u8, u64)> {
    outcome_seq(outcome).map(|seq| (position, seq))
}

/// The §4 boundary commit of one parked child, reported as a normalized
/// outcome: the lease owner on the CAS is the opener driver's identity, so
/// this must run on the opener host, never the successor.
pub(super) async fn commit_group_child(
    controller: &Arc<dyn RuntimeEffectController>,
    scope_id: &str,
    group_key: &str,
    position: u8,
) -> serde_json::Value {
    let replay_key = format!("{group_key}:child:{position}");
    let result = controller
        .commit_group_child_final(GroupChildFinalCommit {
            scope_id: scope_id.to_string(),
            replay_key,
            drain_input: format!("{group_key}:child:{position}:drain-input"),
        })
        .await;
    match result {
        Ok(EffectGroupChildCommitOutcome::Ungrouped) => {
            serde_json::json!({"outcome": "ungrouped"})
        }
        Ok(EffectGroupChildCommitOutcome::Committed { commit_seq, .. }) => {
            serde_json::json!({"outcome": "committed", "commit_seq": commit_seq})
        }
        Ok(EffectGroupChildCommitOutcome::AlreadyCommitted { commit_seq, .. }) => {
            serde_json::json!({"outcome": "already_committed", "commit_seq": commit_seq})
        }
        Ok(EffectGroupChildCommitOutcome::CancelDecided { commit_seq, .. }) => {
            serde_json::json!({"outcome": "cancel_decided", "commit_seq": commit_seq})
        }
        Err(error) => serde_json::json!({"error": neutral_error_code(&error)}),
    }
}

/// Wait until the child's executor has been entered — which is after the
/// claim row was written, so a subsequent §4 commit or drain sees the row.
pub(super) async fn wait_group_child_started(
    executors: &DifferentialGroupExecutors,
    group_key: &str,
    position: u8,
) -> bool {
    let deadline = std::time::Instant::now() + GROUP_OP_BOUND;
    while !executors.started(group_key, usize::from(position)) {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(GROUP_POLL).await;
    }
    true
}

impl GroupOpener {
    /// Send one command and wait out its reply. A dead opener answers the
    /// fixed refusal rather than a channel error, so both backends report the
    /// crash identically.
    pub(super) async fn send(&self, command: GroupOpCommand) -> GroupOpReply {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let Some(commands) = &self.commands else {
            return GroupOpReply {
                outcome: serde_json::json!({"error": "opener_crashed"}),
                committed: Vec::new(),
            };
        };
        if commands.send((command, reply_tx)).is_err() {
            return GroupOpReply {
                outcome: serde_json::json!({"error": "opener_crashed"}),
                committed: Vec::new(),
            };
        }
        match tokio::time::timeout(GROUP_OP_BOUND, reply_rx).await {
            Ok(Ok(reply)) => reply,
            _ => GroupOpReply {
                outcome: serde_json::json!({"error": "opener_timeout"}),
                committed: Vec::new(),
            },
        }
    }

    /// Kill the opener: close the command loop, shut the runtime down and
    /// join the thread. Claims the opener held stop renewing here and lapse
    /// within the group lease TTL. The join is offloaded so a tokio worker
    /// never blocks on the opener's teardown.
    pub(super) async fn crash(&mut self) {
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }

    /// Blocking teardown for `Drop`: drop the command channel so the dispatch
    /// loop exits, then join the thread while the runtime aborts its parked
    /// children (bounded by `shutdown_timeout`).
    pub(super) fn shutdown(&mut self) {
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for GroupOpener {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The runner-visible state of one opened group: what apply needs to answer
/// backend-identically without consulting the opener.
pub(super) struct GroupBook {
    pub(super) crashed: bool,
    pub(super) cancel_losers: bool,
    pub(super) commit_seqs: BTreeMap<u8, u64>,
}
