//! Live upgrade-node plumbing shared by H3's S13/S21/S22 cases: the leased
//! file-SQLite case, the separately materialized candidate/synthetic-next
//! pair, owned host processes and the retained case receipt.
use anyhow::{Context, Result, ensure};
use lash_core::tool_run::{SealOutcome, SourceSeal};
use lash_upgrade_harness::e2e::{
    case::{ArtifactIdentity, CaseLease, CaseSpec},
    control::{CleanupReceipt, ProcessReceipt},
    evidence::{CaseReceipt, Evidence, Verdict},
};
use lash_upgrade_harness::harness::{
    Case, NodeBinary, NodeBuilds, ServeOptions, Services, ServingNode, block_on, wait_for,
};
use lash_upgrade_harness::node::h3::live::SourceOp;
use lash_upgrade_harness::node::h3::{H3Command, SourceSealReply};
use lash_upgrade_harness::restate_view::Invocation;
use serde_json::{Value, json};

pub struct Live {
    pub case: Case,
    pub lease: CaseLease,
    pub builds: NodeBuilds,
    pub spec: CaseSpec,
    pub evidence: Evidence,
    controls: usize,
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

fn sql(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

impl Live {
    /// Lease the case, expand the successor's store shape before any host
    /// holds the file store open, and validate the catalogue spec over the
    /// two separately materialized builds.
    pub fn setup(id: &str, spec: impl FnOnce(Vec<ArtifactIdentity>) -> CaseSpec) -> Result<Self> {
        let root = std::path::PathBuf::from(
            std::env::var_os("LASH_PHASE_A_ARTIFACT_DIR")
                .context("persistent scenario artifacts")?,
        )
        .join(format!("{id}-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        let lease = CaseLease::new(
            id,
            root.join("lease"),
            std::time::Instant::now() + std::time::Duration::from_secs(600),
        )?;
        let case = Case::leased_sqlite(id, &services()?, &lease)?;
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
        let spec = spec(artifacts);
        spec.validate()?;
        let evidence = Evidence::empty(spec.id.clone());
        let live = Self {
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

    /// The journal rows of `invocation`, read verbatim from `sys_journal`.
    pub fn journal(&mut self, invocation: &str) -> Result<()> {
        let rows: Vec<Value> = block_on(self.case.view()?.query(&format!(
            "SELECT index, entry_type, name, version, entry_json FROM sys_journal WHERE id = {} \
             ORDER BY index",
            sql(invocation)
        )))?;
        ensure!(!rows.is_empty(), "{invocation} has no journal rows");
        self.record(
            &format!("journal-{invocation}.json"),
            &Value::Array(rows.clone()),
        )?;
        self.evidence
            .stores
            .push(json!({"journal": invocation, "rows": rows}));
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
