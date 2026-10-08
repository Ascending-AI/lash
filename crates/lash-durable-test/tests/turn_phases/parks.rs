//! The L3 turns whose session parks instead of finishing: every pass fails
//! alike, with an error no pass of this build clears and that is not the
//! turn's own refusal, and the session parks at the activation-loop budget
//! (FIG-5230) with the turn kept.

use super::*;

/// FIG-5398: L3's turn under [`Mode::OutsideWriterRange`], whose head
/// commit the store refuses for the deployment, not for the turn's content.
struct RefusedForTheDeployment(L3);

#[async_trait::async_trait]
impl Scenario for RefusedForTheDeployment {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        self.0.database(clock).await
    }

    fn config(&self) -> SimNodesConfig {
        self.0.config()
    }

    fn activation(&self) -> Arc<dyn Activation> {
        self.0.activation()
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let backend = self.0.backend.lock_recover().clone().expect("the backend");
        seed::send_turn(&backend, &session(), &run(), "think twice").await?;
        nodes.start("a");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![actor()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        matches!(
            nodes.database().actor(&actor()).await,
            Ok(Some(snapshot))
                if matches!(snapshot.state, ActorState::Parked | ActorState::Idle)
        )
    }

    /// The refusal names the deployment, so no pass of this build clears
    /// it and none is the turn's fault: the session parks at the
    /// activation-loop budget with the refusal as its reason, the turn stays
    /// open for a pass after `lashctl finalize` or a rollback, the run has
    /// no end, and the head does not move.
    async fn check(&self, nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
        let database = nodes.database();
        let backend = self.0.backend.lock_recover().clone().expect("the backend");
        let budget = backend.config().settings().activation_loop_budget;
        let mut violations = Vec::new();
        match database.actor(&actor()).await {
            Ok(Some(snapshot)) => {
                if snapshot.state != ActorState::Parked {
                    violations.push(format!("the session is {:?}, not parked", snapshot.state));
                }
                match snapshot
                    .park
                    .as_deref()
                    .map(serde_json::from_str::<SessionParkReason>)
                {
                    Some(Ok(SessionParkReason::PassLoop {
                        failed_passes,
                        error,
                    })) if failed_passes == budget
                        && error.contains("is outside the writer range") => {}
                    other => violations.push(format!(
                        "the session parked for {other:?}, not after {budget} writer-range refusals"
                    )),
                }
            }
            other => violations.push(format!("the session's actor is gone: {other:?}")),
        }
        if !matches!(database.turn(&session()).await, Ok(Some(row)) if row.run == run()) {
            violations.push("the refused turn is no longer open".to_owned());
        }
        match database.turn_end(&session(), &run()).await {
            Ok(None) => {}
            other => violations.push(format!("the run ended: {other:?}")),
        }
        let head = backend
            .session_store_factory()
            .load_session_head_meta(&session())
            .await;
        if !matches!(&head, Ok(head) if head.as_ref().is_none_or(|head| head.leaf_node_id.is_none()))
        {
            violations.push(format!("the refused commit moved the head: {head:?}"));
        }
        violations
    }
}

/// FIG-5398: a turn whose head commit the store refuses for the deployment
/// (a plugin writer format outside the fleet record's range) is not refused:
/// its session parks at the activation-loop budget with the turn open.
#[tokio::test]
async fn a_turn_whose_commit_meets_a_deployment_refusal_parks_instead_of_ending_refused() {
    Matrix::new()
        .faults(&[])
        .horizon(Duration::from_secs(600))
        .run_test(|| {
            RefusedForTheDeployment(L3::new(
                Mode::OutsideWriterRange,
                Dialect::SqliteMemory,
                None,
            ))
        })
        .await
        .assert_held();
}

/// C1 (FIG-5230): a session whose unfinished turn names a checkpoint no
/// build decodes. Its first claim leaves that row, as a defect would; from
/// then on every pass of the production activation fails restoring it.
struct Poisoned {
    dialect: Dialect,
    postgres_url: Option<String>,
    tripwire: Arc<Tripwire>,
    backend: Arc<Mutex<Option<Backend>>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl Poisoned {
    fn new(dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            dialect,
            postgres_url,
            tripwire: Arc::default(),
            backend: Arc::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }
}

/// The session activation, after the first claim writes the poisoned row.
struct PoisonFirst {
    poisoned: std::sync::atomic::AtomicBool,
    session: SessionActivation,
}

#[async_trait::async_trait]
impl Activation for PoisonFirst {
    async fn activate(&self, owned: lash_durable::runner::Owned) -> lash_durable::runner::Exit {
        if !self
            .poisoned
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let mut tx = owned
                .begin()
                .await
                .expect("the first claim reads its actor");
            tx.write(DomainWrite::Turn(TurnWrite::Admit {
                session: session(),
                run: run(),
                admission: RunAdmissionRecord::Turn {
                    took: AdmittedTurnRows::Batch {
                        id: lash_core::BatchId::from("poisoned-batch"),
                    },
                },
                turn_deadline: None,
            }));
            tx.write(DomainWrite::Turn(TurnWrite::Advance {
                session: session(),
                run: run(),
                phase: UnfinishedPhase::Tools {
                    run: lash_durable::domain::RunSeq(1),
                    checkpoint: "not a turn checkpoint".to_owned(),
                },
                iteration: 0,
            }));
            owned
                .commit(tx, CommitLabel::TURN_ADMIT)
                .await
                .expect("the poisoned row commits");
        }
        self.session.activate(owned).await
    }
}

#[async_trait::async_trait]
impl Scenario for Poisoned {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let (stores, database) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        *self.backend.lock_recover() = Some(Backend::for_testing(stores));
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: self.backend().formats().decodes(),
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        Arc::new(PoisonFirst {
            poisoned: std::sync::atomic::AtomicBool::new(false),
            session: SessionActivation::new(
                self.backend(),
                Arc::new(L3Services {
                    mode: Mode::Plain,
                    seen: Arc::default(),
                    backend: Arc::clone(&self.backend),
                }),
                Arc::clone(&self.tripwire) as _,
            ),
        })
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        seed::send_turn(&self.backend(), &session(), &run(), "never runs").await?;
        nodes.start("a");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![actor()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        matches!(
            nodes.database().actor(&actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Parked
        )
    }

    async fn check(&self, nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
        let budget = self.backend().config().settings().activation_loop_budget;
        let snapshot = match nodes.database().actor(&actor()).await {
            Ok(Some(snapshot)) => snapshot,
            other => return vec![format!("the session's actor is gone: {other:?}")],
        };
        let mut violations = Vec::new();
        if snapshot.state != ActorState::Parked {
            violations.push(format!("the session is {:?}, not parked", snapshot.state));
        }
        let reason = snapshot
            .park
            .as_deref()
            .map(serde_json::from_str::<SessionParkReason>);
        match reason {
            Some(Ok(SessionParkReason::PassLoop {
                failed_passes,
                error,
            })) if failed_passes == budget && error.contains("does not decode") => {}
            other => violations.push(format!(
                "the session parked for {other:?}, not after {budget} undecodable restores"
            )),
        }
        violations
    }
}

/// C1 (FIG-5230): a session whose checkpoint does not decode fails every
/// pass, and parks at the activation-loop budget instead of looping on its
/// claim.
async fn prove_poisoned(dialect: Dialect) {
    let postgres_url = match dialect {
        Dialect::Postgres => match dialect::postgres_url() {
            Some(url) => Some(url),
            None => {
                eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
                return;
            }
        },
        Dialect::SqliteMemory | Dialect::SqliteFile => None,
    };
    let report = Matrix::new()
        .faults(&[])
        .horizon(Duration::from_secs(600))
        .run_test(|| Poisoned::new(dialect, postgres_url.clone()))
        .await;
    report.assert_held();
}

#[tokio::test]
async fn an_undecodable_checkpoint_parks_the_session_after_the_budget() {
    prove_poisoned(Dialect::SqliteMemory).await;
}

#[tokio::test]
async fn an_undecodable_checkpoint_parks_the_session_after_the_budget_on_sqlite_file() {
    prove_poisoned(Dialect::SqliteFile).await;
}

#[tokio::test]
async fn an_undecodable_checkpoint_parks_the_session_after_the_budget_on_postgres() {
    prove_poisoned(Dialect::Postgres).await;
}
