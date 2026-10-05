//! Live upgrade-node plumbing shared by H3's S13/S21/S22 cases: the leased
//! case over its permutation's store (file SQLite or the case's own
//! PostgreSQL database), the separately materialized candidate/synthetic-next
//! pair, owned host processes and the retained case receipt.
use anyhow::{Context, Result, ensure};
use lash_core::tool_run::{SealOutcome, SourceSeal};
use lash_upgrade_harness::e2e::{
    case::{ArtifactIdentity, CaseLease, CaseSpec, Leg, Permutation, StoreKind},
    control::{CleanupReceipt, ProcessReceipt, WorkIdentity},
    evidence::{CaseReceipt, Evidence, JournalFact, Verdict},
};
use lash_upgrade_harness::harness::{
    Case, NodeBinary, NodeBuilds, ServeOptions, Services, ServingNode, block_on, wait_for,
};
use lash_upgrade_harness::node::h3::live::SourceOp;
use lash_upgrade_harness::node::h3::{H3Command, SourceSealReply};
use lash_upgrade_harness::restate_view::Invocation;
use serde_json::{Value, json};

pub struct Live {
    pub permutation: Permutation,
    pub case: Case,
    pub lease: CaseLease,
    pub builds: NodeBuilds,
    pub spec: CaseSpec,
    pub evidence: Evidence,
    controls: usize,
}

/// The H3 receipt must carry journal provenance that the runner can certify.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_collection_retains_typed_v7_evidence() -> Result<()> {
    use lash_upgrade_harness::e2e::case::Leg;
    use lash_upgrade_harness::identity::BuildLabel;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let root = tempfile::tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let admin_url = format!("http://{}", listener.local_addr()?);
    let lease = CaseLease {
        gate_id: "journal-law".into(),
        namespace: "e2e-journal-law".into(),
        authority: "journal-law".into(),
        directory: root.path().into(),
        postgres_url: None,
        ports: Vec::new(),
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
        processes: Vec::new(),
        cleanup: Vec::new(),
    };
    let case = Case::leased_sqlite(
        "journal-law",
        &Services {
            ingress_url: admin_url.clone(),
            admin_url: admin_url.clone(),
            postgres_url: String::new(),
        },
        &lease,
    )?;
    async fn serve_queries(listener: tokio::net::TcpListener) -> Result<()> {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut request = Vec::new();
            let body = loop {
                let mut bytes = [0; 4096];
                let read = stream.read(&mut bytes).await?;
                ensure!(read > 0, "admin request ended before its body");
                request.extend_from_slice(&bytes[..read]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end])?;
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .context("query body length")?
                        .parse()?;
                    if request.len() >= end + 4 + length {
                        break serde_json::from_slice::<Value>(
                            &request[end + 4..end + 4 + length],
                        )?;
                    }
                }
            };
            let query = body["query"].as_str().context("admin SQL query")?;
            let rows = if query.contains("FROM sys_invocation") {
                json!([{"target_service_name":"e2e-journal-law.LashTurn_g1",
                    "pinned_service_protocol_version":7}])
            } else {
                ensure!(
                    query.contains("FROM sys_journal"),
                    "unexpected query: {query}"
                );
                json!([{"index":0,"entry_type":"Input","name":null,"version":2,
                    "entry_json":"{\"Command\":{\"Input\":{}}}"}])
            };
            let response = serde_json::to_vec(&json!({"rows":rows}))?;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).as_bytes()).await?;
            stream.write_all(&response).await?;
        }
    }
    let server = tokio::spawn(serve_queries(listener));
    let spec = super::process::s21_spec(StoreKind::SqliteFile, Vec::new());
    let mut live = Live {
        permutation: Permutation {
            store: StoreKind::SqliteFile,
            leg: Leg::Live,
        },
        case,
        lease,
        builds: NodeBuilds {
            n: NodeBinary::at(std::env::current_exe()?, BuildLabel::N),
            next: NodeBinary::at(std::env::current_exe()?, BuildLabel::Next),
        },
        evidence: Evidence::empty(spec.id.clone()),
        spec,
        controls: 0,
    };
    let collected = tokio::task::spawn_blocking(move || {
        live.journal(
            "inv-journal-law",
            "law-ingress",
            &lash_core::TurnId::fixture("law-run"),
        )?;
        Ok::<_, anyhow::Error>(live.evidence)
    })
    .await?;
    server.abort();
    let evidence = collected?;
    ensure!(
        evidence.journals.len() == 1,
        "H3 receipt lacks typed journal evidence"
    );
    let fact = &evidence.journals[0];
    ensure!(fact.invocation == "inv-journal-law" && fact.index == 0);
    ensure!(fact.protocol == 7 && fact.admin_url == admin_url);
    ensure!(
        fact.work
            == WorkIdentity {
                ingress: "law-ingress".into(),
                run: "law-run".into(),
                segment: "inv-journal-law".into(),
                call: None,
                ordinal: None,
            }
    );
    ensure!(fact.value == json!({"Command":{"Input":{}}}));
    Ok(())
}

fn services() -> Result<Services> {
    Ok(Services {
        ingress_url: std::env::var("RESTATE_INGRESS_URL")
            .context("private live Restate ingress")?,
        admin_url: std::env::var("RESTATE_ADMIN_URL").context("private live Restate admin")?,
        postgres_url: String::new(),
    })
}

/// Decode a JSON control into the node's typed command.
pub fn command(value: Value) -> Result<H3Command> {
    serde_json::from_value(value.clone()).with_context(|| format!("H3 command {value}"))
}

/// What an external completion met at the live source.
#[derive(Debug)]
pub enum Completion {
    /// The result was retained and the registry answered its seal write.
    Sealed {
        seal: SourceSeal,
        reply: SourceSealReply,
    },
    /// The source's material holder had ended: the store refused the
    /// late result typed before any seal write.
    RetentionRefused(Value),
}

/// A completion after the source sealed keeps `first`: the registry answers
/// the existing seal or refuses typed, or the ended holder refuses the
/// result before it can be sealed.
pub fn kept(completion: &Completion, first: &SourceSeal) -> bool {
    match completion {
        Completion::Sealed {
            reply:
                SourceSealReply::Outcome {
                    outcome: SealOutcome::AlreadySealed { seal },
                },
            ..
        } => seal == first,
        Completion::Sealed {
            reply: SourceSealReply::Refused { .. },
            ..
        } => true,
        Completion::RetentionRefused(refusal) => refusal["kind"] == "HolderEnded",
        Completion::Sealed { .. } => false,
    }
}

/// The runner-served Restate's Prometheus endpoint: `restate_suite.py serve`
/// binds its ingress, admin and node roles from the gate's port base + 45,
/// and the node port serves the metrics.
fn served_metrics_url() -> Result<String> {
    let base: u16 = std::env::var("LASH_E2E_PORT_BASE")
        .context("runner port base")?
        .parse()?;
    Ok(format!("http://127.0.0.1:{}/metrics", base + 47))
}

/// What a replay-leg journal proves. The always-suspending server ends an
/// attempt at every await, so a Command appended after a Notification was
/// written by a resumed attempt that first replayed every Command recorded
/// before that Notification.
fn replayed(facts: &[JournalFact]) -> Result<Value> {
    let is = |fact: &JournalFact, kind: &str| {
        fact.value
            .as_object()
            .is_some_and(|entry| entry.contains_key(kind))
    };
    let last = facts
        .iter()
        .rposition(|fact| is(fact, "Command"))
        .context("journal records no command")?;
    let resumed = facts[..last]
        .iter()
        .rposition(|fact| is(fact, "Notification"))
        .context("no command follows a notification: the invocation never resumed")?;
    let replayed: Vec<&str> = facts[..resumed]
        .iter()
        .filter(|fact| is(fact, "Command"))
        .map(|fact| fact.entry_type.as_str())
        .collect();
    let appended: Vec<&str> = facts[resumed..]
        .iter()
        .filter(|fact| is(fact, "Command"))
        .map(|fact| fact.entry_type.as_str())
        .collect();
    ensure!(
        replayed.len() > 1,
        "the resumed attempt replayed only its input: {replayed:?}"
    );
    Ok(json!({
        "kind": "replayed_journal",
        "invocation": facts[resumed].invocation,
        "resumed_after": facts[resumed].index,
        "replayed_commands": replayed,
        "appended_commands": appended,
    }))
}

fn sql(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

impl Live {
    /// Lease the case over `permutation`'s store, expand the successor's
    /// store shape before any host holds it open, and validate the catalogue
    /// spec over the two separately materialized builds.
    pub fn setup(
        id: &str,
        permutation: Permutation,
        spec: impl FnOnce(StoreKind, Vec<ArtifactIdentity>) -> CaseSpec,
    ) -> Result<Self> {
        let root = std::path::PathBuf::from(
            std::env::var_os("LASH_PHASE_A_ARTIFACT_DIR")
                .context("persistent scenario artifacts")?,
        )
        .join(format!("{id}-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        let mut lease = CaseLease::new(
            id,
            root.join("lease"),
            std::time::Instant::now() + std::time::Duration::from_secs(600),
        )?;
        let services = services()?;
        let case = match permutation.store {
            StoreKind::SqliteFile => Case::leased_sqlite(id, &services, &lease)?,
            StoreKind::PostgreSql => {
                let url = block_on(permutation.postgres_url(&mut lease))?
                    .context("PostgreSQL permutation provisioned no database")?;
                Case::leased_postgres(id, &services, &lease, &url)?
            }
            StoreKind::SqliteMemory => {
                anyhow::bail!("upgrade-node hosts share a durable store across processes")
            }
        };
        let builds = NodeBuilds::from_env()?;
        let mut artifacts = Vec::new();
        for (role, variable) in [
            ("candidate", lash_upgrade_harness::harness::NODE_N_ENV),
            (
                "synthetic-next",
                lash_upgrade_harness::harness::NODE_NEXT_ENV,
            ),
        ] {
            let path = std::path::PathBuf::from(
                std::env::var_os(variable).context("materialized binary")?,
            );
            artifacts.push(ArtifactIdentity {
                role: role.into(),
                sha256: lash_core::stable_hash::sha256_hex(&std::fs::read(&path)?),
                path,
                candidate_sha: std::env::var("LASH_E2E_CANDIDATE_SHA")?,
                generation: role.into(),
            });
        }
        ensure!(
            artifacts[0].sha256 != artifacts[1].sha256,
            "candidate and successor must be separately materialized builds"
        );
        let spec = spec(permutation.store, artifacts);
        spec.validate()?;
        let evidence = Evidence::empty(spec.id.clone());
        let live = Self {
            permutation,
            case,
            lease,
            builds,
            spec,
            evidence,
            controls: 0,
        };
        live.record("case-spec.json", &serde_json::to_value(&live.spec)?)?;
        live.record(
            "store-expand.json",
            &serde_json::to_value(live.builds.next.probe(&live.case, None)?)?,
        )?;
        Ok(live)
    }

    /// The case's store set, opened beside its hosts for an oracle read.
    pub async fn stores(&self) -> Result<std::sync::Arc<dyn lash::StoreSet>> {
        Ok(match self.permutation.store {
            StoreKind::PostgreSql => {
                let url = self.case.postgres_url().context("the case's database")?;
                let storage = lash_postgres_store::PostgresStorage::connect(url).await?;
                std::sync::Arc::new(lash_postgres_store::PostgresStoreSet::new(
                    &storage,
                    std::sync::Arc::new(lash::persistence::FileAttachmentStore::new(
                        self.lease.directory.join("oracle-attachments"),
                    )),
                ))
            }
            StoreKind::SqliteFile | StoreKind::SqliteMemory => {
                let directory = self.case.sqlite_dir().context("the case's SQLite store")?;
                std::sync::Arc::new(lash::sqlite::SqliteStoreSet::open(directory).await?)
            }
        })
    }

    pub fn record(&self, name: &str, value: &Value) -> Result<()> {
        std::fs::write(
            self.case.gate_dir().join(name),
            serde_json::to_vec_pretty(value)?,
        )?;
        Ok(())
    }

    /// One H3 control through `node`'s own process; every answer is retained.
    pub fn h3(&mut self, node: &NodeBinary, session: &str, control: &H3Command) -> Result<Value> {
        let answer = node.h3(&self.case, session, control)?;
        self.retain_control(node, session, control, &answer)?;
        Ok(answer)
    }

    pub fn retain_control(
        &mut self,
        node: &NodeBinary,
        session: &str,
        control: &H3Command,
        answer: &Value,
    ) -> Result<()> {
        self.controls += 1;
        self.record(
            &format!("control-{:03}.json", self.controls),
            &json!({"build": node.label(), "session": session, "command": control, "answer": answer}),
        )?;
        self.evidence
            .stores
            .push(json!({"control": control, "answer": answer}));
        Ok(())
    }

    fn note(&mut self, node: &ServingNode, role: &str) -> Result<()> {
        let pid = node.pid()?;
        self.lease
            .ports
            .push(node.bind()?.parse::<std::net::SocketAddr>()?.port());
        let incarnation = self
            .lease
            .processes
            .iter()
            .filter(|process| process.role == role)
            .count() as u32
            + 1;
        self.lease.processes.push(ProcessReceipt {
            role: role.into(),
            pid,
            incarnation,
            log: std::fs::read_link(format!("/proc/{pid}/fd/1"))?
                .display()
                .to_string(),
        });
        Ok(())
    }

    /// Serve `node` registered at a fresh URI.
    pub fn serve(&mut self, node: &NodeBinary, role: &str) -> Result<ServingNode> {
        let serving = node.serve(&self.case)?;
        self.note(&serving, role)?;
        Ok(serving)
    }

    /// Serve `node` unregistered behind a crashed host's address.
    pub fn serve_at(&mut self, node: &NodeBinary, bind: &str, role: &str) -> Result<ServingNode> {
        let serving = node.serve_with(
            &self.case,
            &ServeOptions {
                bind: Some(bind.to_owned()),
                unregistered: true,
                ..ServeOptions::default()
            },
        )?;
        self.note(&serving, role)?;
        Ok(serving)
    }

    /// SIGKILL an owned host and retain the fault.
    pub fn kill(&mut self, mut node: ServingNode, role: &str, cut: &str) -> Result<()> {
        let pid = node.pid()?;
        node.kill_and_reap()?;
        let fault = json!({"fault": "SIGKILL", "target": role, "pid": pid, "cut": cut});
        self.record(&format!("kill-{role}-{pid}.json"), &fault)?;
        self.evidence.stores.push(fault);
        self.stopped(role, "owned host killed with SIGKILL and reaped");
        Ok(())
    }

    pub fn stop(&mut self, node: ServingNode, role: &str) -> Result<()> {
        node.stop()?;
        self.stopped(role, "owned host stopped and reaped");
        Ok(())
    }

    fn stopped(&mut self, role: &str, detail: &str) {
        self.lease.cleanup.push(CleanupReceipt {
            resource: role.into(),
            closed: true,
            detail: detail.into(),
        });
    }

    /// Complete `operation`'s source externally through `node`, answering
    /// the seal written and the registry's reply.
    pub fn complete(
        &mut self,
        node: &NodeBinary,
        session: &str,
        operation: &str,
        output: &str,
    ) -> Result<Completion> {
        let answer = self.h3(
            node,
            session,
            &H3Command::Source {
                operation: operation.into(),
                op: SourceOp::Complete {
                    output: json!(output),
                },
            },
        )?;
        if let Some(refusal) = answer.get("retention_refused") {
            return Ok(Completion::RetentionRefused(refusal.clone()));
        }
        Ok(Completion::Sealed {
            seal: serde_json::from_value(answer["seal"].clone())?,
            reply: serde_json::from_value(answer["reply"].clone())?,
        })
    }

    /// The first completion of a pending source seals it.
    pub fn complete_first(
        &mut self,
        node: &NodeBinary,
        session: &str,
        operation: &str,
        output: &str,
    ) -> Result<SourceSeal> {
        match self.complete(node, session, operation, output)? {
            Completion::Sealed {
                seal,
                reply:
                    SourceSealReply::Outcome {
                        outcome: SealOutcome::Sealed { seal: sealed },
                    },
            } if sealed == seal => Ok(seal),
            other => {
                anyhow::bail!("the pending source did not seal its first completion: {other:?}")
            }
        }
    }

    /// Every physical `run` journal of a Run executor key, on any
    /// generation-suffixed turn service of the case namespace.
    pub fn run_invocations(&self, key: &str) -> Result<Vec<Invocation>> {
        let view = self.case.view()?;
        block_on(view.query(&format!(
            "SELECT id, status, pinned_deployment_id, invoked_by_id, last_failure, retry_count \
             FROM sys_invocation WHERE target_service_name LIKE {} AND target_service_key = {} \
             AND target_handler_name = 'run' ORDER BY created_at",
            sql(&format!("{}%", view.service_name("LashTurn"))),
            sql(key)
        )))
    }

    /// The one physical journal of an operation Run, once it is suspended.
    pub fn await_suspended(
        &self,
        node: &NodeBinary,
        session: &str,
        run: &lash_core::TurnId,
    ) -> Result<(String, Invocation)> {
        let snapshot = H3Command::Snapshot { run: run.clone() };
        let key = wait_for(&format!("Run {run}'s executor admission"), || {
            Ok(node.h3(&self.case, session, &snapshot)?["invocation_key"]
                .as_str()
                .map(str::to_owned))
        })?;
        let invocation = wait_for(&format!("Run {run} to suspend on its source"), || {
            let invocations = self.run_invocations(&key)?;
            ensure!(
                invocations.len() <= 1,
                "Run {run} has several physical journals: {invocations:?}"
            );
            Ok(invocations
                .into_iter()
                .find(|invocation| invocation.status == "suspended"))
        })?;
        Ok((key, invocation))
    }

    /// The invocation's decoded journal, with verified V7 and admin provenance.
    pub fn journal(
        &mut self,
        invocation: &str,
        ingress: &str,
        run: &lash_core::TurnId,
    ) -> Result<()> {
        let work = WorkIdentity {
            ingress: ingress.into(),
            run: run.to_string(),
            segment: invocation.into(),
            call: None,
            ordinal: None,
        };
        let facts = block_on(self.case.view()?.journal(&work, invocation, 7))?;
        ensure!(!facts.is_empty(), "{invocation} has no journal rows");
        self.record(
            &format!("journal-{invocation}.json"),
            &serde_json::to_value(&facts)?,
        )?;
        if self.permutation.leg == Leg::Replay {
            let replay =
                replayed(&facts).with_context(|| format!("{invocation} never replayed"))?;
            self.record(&format!("replay-{invocation}.json"), &replay)?;
            self.evidence.stores.push(replay);
        }
        self.evidence.journals.extend(facts);
        Ok(())
    }

    /// No invocation of the namespace remains open.
    pub fn quiesce(&self) -> Result<()> {
        let view = self.case.view()?;
        wait_for("every case invocation to settle", || {
            Ok(block_on(view.open_invocations())?.is_empty().then_some(()))
        })
    }

    pub fn finish(mut self) -> Result<()> {
        let leg = block_on(lash_upgrade_harness::e2e::cluster::observe_leg(
            self.permutation.leg,
            &[served_metrics_url()?],
            &self.case.gate_dir(),
        ))?;
        self.evidence.stores.push(leg);
        let lease = &self.lease;
        self.record(
            "ownership.json",
            &json!({"gate":lease.gate_id,"namespace":lease.namespace,"authority":lease.authority,"ports":lease.ports,"processes":lease.processes,"cleanup":lease.cleanup}),
        )?;
        self.evidence.artifacts = self.spec.artifacts.clone();
        self.evidence.cleanup = self.lease.cleanup.clone();
        CaseReceipt {
            evidence: self.evidence,
            verdict: Verdict::Passed,
        }
        .write(&self.case.gate_dir())?
        .reconcile()
    }
}
