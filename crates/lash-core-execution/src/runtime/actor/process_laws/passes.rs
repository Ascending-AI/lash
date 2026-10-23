use super::*;

use crate::testing::DurableCommitHook;
use lash_core_store::checkpoint_graph::CommitId;
use lash_core_store::checkpoint_graph::DecodeLimits;
use lash_core_store::checkpoint_graph::EncodeBudget;
use lash_core_store::checkpoint_graph::EncodedNode;
use lash_core_store::checkpoint_graph::ExplicitLimit;
use lash_core_store::checkpoint_graph::GraphStaging;
use lash_core_store::checkpoint_graph::NodeId;
use lash_core_store::checkpoint_graph::NodeKind;
use lash_core_store::checkpoint_graph::TypedBody;
use lash_core_store::checkpoint_graph::node::VerifiedNode;
use lash_core_store::checkpoint_graph::stage::{
    CandidateManifest, EdgeLog, ManifestEntry, ManifestSummary, MemoryEdgeLog, StageFault,
    node_range,
};
use lash_durable::domain::{OwnerKey, RunFactKind, RunFactWrite};
use lash_durable::{CommitLabel, DomainWrite};
use lash_execution_contracts::process::{StepCandidate, StepManifest, StepPublication};
use lash_vocabulary::tokens::TokenWriter;

/// How many outcomes `tx` records.
fn outcomes(tx: &lash_durable::ActorTx) -> usize {
    tx.domain()
        .iter()
        .filter(|write| {
            matches!(
                write,
                DomainWrite::RunFact(RunFactWrite::Append {
                    kind: RunFactKind::XOutcome,
                    ..
                })
            )
        })
        .count()
}

/// Every step `process` admitted entered its body exactly once and
/// completed, and there are `count` of them.
async fn each_ran_once(backend: &Backend, process: &ProcessId, count: usize) -> LawOutcome {
    let rows = backend
        .durable()
        .run_facts(&OwnerKey::Process(process.clone()))
        .await?;
    let waits = round::PinnedWaits::read(backend.durable().as_ref(), &rows).await?;
    let fold = round::fold(&rows, &PolicyView::default(), &waits)
        .map_err(|refusal| LawBroken(refusal.to_string()))?;
    let members: Vec<_> = fold
        .rounds()
        .flat_map(|view| view.members().to_vec())
        .collect();
    ensure!(
        members.len() == count,
        "the process admitted {} steps, not {count}",
        members.len()
    );
    for member in &members {
        ensure!(
            matches!(
                member.state(),
                MemberState::Final {
                    outcome: SettledOutput::Completed(_),
                    ..
                }
            ),
            "step {} ended {:?}, not completed",
            member.call(),
            member.state()
        );
        ensure!(
            step_entries(member.call()) == 1,
            "step {}'s body was entered {} times, not once",
            member.call(),
            step_entries(member.call())
        );
    }
    Ok(())
}

/// FIG-7046: a process owner collects every step completion already ready
/// when the first one wakes it, without waiting for one still running, and
/// commits them together, at most the group commit's member bound to one
/// outcome transaction. Ten steps whose bodies all finish together, under
/// a bound of four and no batch window, settle in three outcome commits of
/// four, four and two, each body entered once; collecting one completion
/// per wake commits ten of one.
///
/// # Errors
///
/// The first rule broken.
pub async fn ready_step_completions_commit_together_up_to_the_member_bound(
    backend: &Backend,
) -> LawOutcome {
    const STEPS: usize = 10;
    let mut settings = settings();
    settings.group_commit.max_members = 4;
    settings.group_commit.window = Duration::ZERO;
    let backend = Backend::assemble(BackendParts {
        formats: Vec::new(),
        stores: backend.stores(),
        settings,
        engines: vec![Arc::new(LawEngine { version: 0 })],
        providers: Arc::new(lash_execution_contracts::projection::NoProjectionProviders),
    })
    .map_err(|error| LawBroken(error.to_string()))?;
    let tag = tag("fan");
    READY_GATES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(tag.clone(), Arc::new(tokio::sync::Barrier::new(STEPS)));
    let hook = DurableCommitHook::over(Arc::clone(backend.durable()));
    let process = root(
        &backend,
        json!({ "tag": tag, "act": "fan", "steps": STEPS }),
    )
    .await?;
    let key = actor(&process)?;
    let batches: Arc<Mutex<Vec<usize>>> = Arc::default();
    {
        let batches = Arc::clone(&batches);
        hook.arm(move |tx, label| {
            if label == CommitLabel::STEP_OUTCOME && tx.actor() == &key {
                batches
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(outcomes(tx));
            }
            std::future::ready(Ok(()))
        });
    }
    let serving = serve_over(&backend, NODE, Vec::new(), Arc::new(hook));
    let result = async {
        eventually(SETTLE, "the fanned-out process ended", || async {
            Ok(terminal(&backend, &process).await?.is_some())
        })
        .await?;
        let batches = batches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        ensure!(
            batches == [4, 4, 2],
            "ten ready completions committed as outcome batches {batches:?}, not [4, 4, 2]"
        );
        each_ran_once(&backend, &process, STEPS).await
    }
    .await;
    serving.stop().await;
    result
}

/// LO-POSTIMAGE: after acknowledged transitions a process never reopens its
/// record, driver or root, and cold recovery reads no completed runs. A hidden
/// outcome acknowledgement discards the whole projection, preserving Once IDs.
///
/// # Errors
/// The first rule broken.
pub async fn lo_process_retains_acknowledged_state(backend: &Backend) -> LawOutcome {
    for lose in [false, true] {
        const STEPS: u64 = 10;
        let backend = law_backend(backend)?;
        let hook = DurableCommitHook::over(Arc::clone(backend.durable()));
        let process = root(
            &backend,
            json!({ "tag": tag("retained"), "act": "chain", "steps": STEPS }),
        )
        .await?;
        let key = actor(&process)?;
        let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if lose {
            let store = Arc::clone(backend.durable());
            let lost = Arc::clone(&lost);
            hook.arm(move |tx, label| {
                let hide = label == CommitLabel::STEP_OUTCOME
                    && tx.actor() == &key
                    && !lost.swap(true, Ordering::SeqCst);
                let tx = tx.clone();
                let store = Arc::clone(&store);
                async move {
                    if hide {
                        store.commit(tx, label).await?;
                        return Err(DurableError::AckLost { label });
                    }
                    Ok(())
                }
            });
        }
        let serving = serve_over(&backend, NODE, Vec::new(), Arc::new(hook.clone()));
        let result = async {
            eventually(SETTLE, "the retained chain ended", || async {
                Ok(terminal(&backend, &process).await?.is_some())
            })
            .await?;
            ensure!(
                find(
                    &terminal(&backend, &process).await?.unwrap_or_default(),
                    "settled"
                ) == Some(&json!(STEPS)),
                "the chain lost work"
            );
            ensure!(
                hook.calls("open_process") == if lose { 2 } else { 1 },
                "acknowledged state was reopened {} times (lost={lose})",
                hook.calls("open_process")
            );
            ensure!(
                hook.calls("run_facts") == 0,
                "recovery read completed run history"
            );
            ensure!(
                !lose || lost.load(Ordering::SeqCst),
                "lost acknowledgement cut was not reached"
            );
            each_ran_once(&backend, &process, STEPS as usize).await
        }
        .await;
        serving.stop().await;
        result?;
    }
    Ok(())
}

/// A Once body's held outcome survives one refused owner revision and is
/// recorded exactly once against the freshly loaded admission.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_once_process_body_survives_one_revision_conflict(backend: &Backend) -> LawOutcome {
    let backend = law_backend(backend)?;
    let hook = DurableCommitHook::over(Arc::clone(backend.durable()));
    let process = root(
        &backend,
        json!({ "tag": tag("conflict"), "act": "chain", "steps": 1 }),
    )
    .await?;
    let key = actor(&process)?;
    let injected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let injected = Arc::clone(&injected);
        let store = Arc::clone(backend.durable());
        hook.arm(move |tx, label| {
            let intervene = label == CommitLabel::STEP_OUTCOME
                && tx.actor() == &key
                && !injected.swap(true, Ordering::SeqCst);
            let tx = tx.clone();
            let store = Arc::clone(&store);
            async move {
                if intervene {
                    let winner = store.begin(tx.actor(), tx.epoch()).await?;
                    store
                        .commit(winner, CommitLabel::new("law.intervene"))
                        .await?;
                }
                Ok(())
            }
        });
    }
    let serving = serve_over(&backend, NODE, Vec::new(), Arc::new(hook));
    let result = async {
        eventually(SETTLE, "the conflicted process ended", || async {
            Ok(terminal(&backend, &process).await?.is_some())
        })
        .await?;
        ensure!(
            injected.load(Ordering::SeqCst),
            "the outcome commit was not interposed"
        );
        let outcome = terminal(&backend, &process).await?.unwrap_or_default();
        ensure!(
            find(&outcome, "settled") == Some(&json!(1)),
            "the Once body lost its outcome: {outcome}"
        );
        let rows = backend
            .durable()
            .run_facts(&OwnerKey::Process(process.clone()))
            .await?;
        ensure!(
            rows.rows
                .iter()
                .filter(|row| row.kind == RunFactKind::XOutcome)
                .count()
                == 1,
            "the Once body recorded more than one outcome"
        );
        each_ran_once(&backend, &process, 1).await
    }
    .await;
    serving.stop().await;
    result
}

/// PROCESS-TERMINAL-DEBT: a terminal accepts an unlinked process and retires
/// both the registration and materialized claim debt of a linked process.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_process_terminal_retires_its_optional_execution_debt(
    backend: &Backend,
) -> LawOutcome {
    use lash_core_store::artifact_referrer::ArtifactReferrer;
    use lash_durable::resolutions::{ClaimRequirements, ExecutionId, ExecutionRegistration};

    let backend = law_backend(backend)?;
    for linked in [false, true] {
        let process = root(
            &backend,
            json!({ "tag": tag("terminal-debt"), "act": "complete" }),
        )
        .await?;
        let key = actor(&process)?;
        let store = backend.durable();
        let formats = store
            .actor(&key)
            .await?
            .ok_or_else(|| LawBroken("the process has no actor".into()))?
            .formats;
        let node = store
            .register_node(&NodeSpec {
                node: lash_durable::NodeId::new(format!("terminal-debt-{linked}")),
                decodes: vec![formats],
                serves: Vec::new(),
                capabilities: Default::default(),
                ttl_millis: TTL_MILLIS,
            })
            .await?;
        let claimed = store.claim(&node, 1).await?;
        let [claim] = claimed.as_slice() else {
            return Err(LawBroken(format!("one process is claimable: {claimed:?}")));
        };
        ensure!(claim.actor == key, "claimed another process");
        let resolutions = backend.stores().execution_resolutions();
        if linked {
            let mut tx = store.begin(&key, claim.epoch).await?;
            let requirements = ClaimRequirements {
                providers: [lash_vocabulary::providers::ToolProviderHandle::new(
                    lash_vocabulary::claims::RegistryKind::Process,
                    "terminal-law",
                    crate::tool_provider_key!("target"),
                )]
                .into(),
                ..Default::default()
            };
            tx.register_execution(ExecutionRegistration {
                execution: ExecutionId::process(&process),
                uses: Default::default(),
                referrer: ArtifactReferrer::ProcessRecord(process.clone()),
                requirements: requirements.clone(),
            });
            store
                .commit(tx, CommitLabel::new("law.terminal.register"))
                .await?;
            ensure!(
                resolutions.requirements(&key).await? == requirements,
                "registration lost its debt"
            );
        }
        let mut tx = store.begin(&key, claim.epoch).await?;
        let outcome = ProcessOutcome::from_tool_output(ToolCallOutput::success(json!({})));
        super::super::process::record_terminal(&mut tx, &process, &outcome)?;
        store.commit(tx, CommitLabel::PROCESS_TERMINAL).await?;
        ensure!(
            terminal(&backend, &process).await?.is_some(),
            "the terminal did not commit"
        );
        ensure!(
            resolutions.requirements(&key).await? == ClaimRequirements::default(),
            "terminal retained execution debt"
        );
        ensure!(
            store
                .actor_requirements(None, 8)
                .await?
                .iter()
                .all(|(actor, debt)| actor != &key || debt.is_empty()),
            "terminal retained materialized claim debt"
        );
        let mut cascade = store.begin(&key, claim.epoch).await?;
        cascade.give_up(lash_durable::Release::Terminal);
        store
            .commit(cascade, CommitLabel::new("law.terminal.cascade"))
            .await?;
        store.release_node(&node).await?;
    }
    Ok(())
}

/// The CheckpointRoot page a `law_paged` step of call `call` publishes:
/// one page of its own, named by its outcome and supplied only by its
/// candidate's manifest, as a kernel run's parked root is.
#[expect(
    clippy::expect_used,
    reason = "a law's fixed one-page body always encodes"
)]
fn law_root(call: &ToolCallId) -> VerifiedNode {
    let mut body = TokenWriter::new();
    body.seq(1);
    body.text(call.as_str());
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let id = staging
        .encode(
            NodeKind::CHECKPOINT_ROOT,
            TypedBody::logical(body.as_bytes()),
        )
        .expect("a law's page encodes");
    let page = staging.get(&id).cloned().expect("a law's page is staged");
    VerifiedNode::check(page, &DecodeLimits::within_total(u64::MAX)).expect("a law's page checks")
}

/// What a `law_paged` step's outcome publishes: its one root page.
struct LawPages {
    root: VerifiedNode,
}

#[async_trait::async_trait]
impl StepPublication for LawPages {
    fn root(&self) -> NodeId {
        self.root.id
    }

    async fn publication(
        &self,
        _tx: &mut lash_durable::ActorTx,
        commit: CommitId,
    ) -> Result<StepManifest, DurableError> {
        let manifest = Box::new(OnePage {
            summary: ManifestSummary {
                root: self.root.id,
                base: None,
                commit,
                fresh_nodes: 1,
                fresh_bytes: self.root.len(),
                fresh_edges: 0,
                external: 0,
            },
            root: self.root.clone(),
        });
        Ok(StepManifest {
            manifest,
            root: EncodedNode::clone(&self.root),
        })
    }

    async fn settled(&self, _commit: CommitId) {}

    async fn withdrawn(&self, _commit: CommitId) {}

    async fn abandoned(&self) {}
}

/// The manifest of a candidate that is its root page alone.
struct OnePage {
    summary: ManifestSummary,
    root: VerifiedNode,
}

impl CandidateManifest for OnePage {
    fn summary(&self) -> ManifestSummary {
        self.summary
    }

    fn entries(
        &mut self,
        after: Option<NodeId>,
        limit: usize,
    ) -> Result<Vec<ManifestEntry>, StageFault> {
        Ok(
            (limit > 0 && after.is_none_or(|after| self.root.id > after))
                .then(|| ManifestEntry::Fresh(self.root.clone()))
                .into_iter()
                .collect(),
        )
    }

    fn fresh_range(&mut self, id: &NodeId, offset: u64, len: usize) -> Result<Vec<u8>, StageFault> {
        if *id == self.root.id {
            node_range(&self.root, offset, len)
        } else {
            Err(StageFault::Missing(*id))
        }
    }

    fn edge_log(&mut self) -> Result<Box<dyn EdgeLog>, StageFault> {
        Ok(Box::new(MemoryEdgeLog::default()))
    }
}

/// The body of `process`'s `law_paged` step `call`: it completes at once,
/// publishing its root page with its outcome.
pub(super) fn paged_body(process: ProcessId, call: ToolCallId) -> round::MemberBody {
    Box::new(move |_token| {
        Box::pin(async move {
            *STEP_ENTRIES
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(call.as_str().to_owned())
                .or_default() += 1;
            round::MemberOutcome {
                candidate: Some(StepCandidate(Arc::new(LawPages {
                    root: law_root(&call),
                }))),
                ..SettledOutput::Completed(step_output(&process, "paged")).into()
            }
        })
    })
}

/// FIG-7063: what a process owner keeps of its own acknowledged commits
/// decodes as the store's read of them does. A step outcome that publishes
/// heap pages names a CheckpointRoot that only its candidate supplies, not
/// its transaction's staged nodes; the owner keeps that page with the
/// outcome's fact, so its next pass folds the outcome without reading the
/// run again. A chain of steps that each publish pages settles every step
/// once, restoring its facts once and reading no run's facts again; an
/// owner that kept only the staged nodes refuses its own outcome as
/// undecodable.
///
/// # Errors
///
/// The first rule broken.
pub async fn lo_process_retains_candidate_pages(backend: &Backend) -> LawOutcome {
    const STEPS: u64 = 4;
    let backend = law_backend(backend)?;
    let hook = DurableCommitHook::over(Arc::clone(backend.durable()));
    let tag = tag("paged");
    let process = root(
        &backend,
        json!({ "tag": tag, "act": "paged_chain", "steps": STEPS }),
    )
    .await?;
    let serving = serve_over(&backend, NODE, Vec::new(), Arc::new(hook.clone()));
    let result = async {
        eventually(SETTLE, "the paged chain ended", || async {
            Ok(terminal(&backend, &process).await?.is_some())
        })
        .await?;
        let outcome = terminal(&backend, &process).await?.unwrap_or_default();
        ensure!(
            find(&outcome, "settled") == Some(&json!(STEPS)),
            "the paged chain ended as {outcome}, not after its {STEPS} steps"
        );
        ensure!(
            hook.calls("open_process") == 1 && hook.calls("run_facts") == 0,
            "candidate outcomes required an own-state/history reload: opens={}, history={}",
            hook.calls("open_process"),
            hook.calls("run_facts")
        );
        each_ran_once(&backend, &process, usize::try_from(STEPS).unwrap_or(0)).await
    }
    .await;
    serving.stop().await;
    result
}

/// LO-POSTIMAGE: the commit returns the exact realized record, guarded driver
/// and root postimage, including external changes after the owner's open.
///
/// # Errors
/// The first rule broken.
pub async fn lo_process_receipt_is_exact(backend: &Backend) -> LawOutcome {
    let backend = law_backend(backend)?;
    let process = root(&backend, json!({"tag": tag("receipt"), "act": "hold"})).await?;
    let key = actor(&process)?;
    let store = backend.durable();
    let formats = store
        .actor(&key)
        .await?
        .ok_or_else(|| LawBroken("missing actor".into()))?
        .formats;
    let node = store
        .register_node(&NodeSpec {
            node: lash_durable::NodeId::new("process-receipt"),
            decodes: vec![formats],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: TTL_MILLIS,
        })
        .await?;
    let claimed = store.claim(&node, 1).await?;
    let [claim] = claimed.as_slice() else {
        return Err(LawBroken("one claim required".into()));
    };
    // Public nonretained commits keep the same receipt contract; PostgreSQL
    // can use its generated function for this authority-only transition.
    let plain = store.begin(&key, claim.epoch).await?;
    let plain = store.commit(plain, CommitLabel::PROCESS_ADVANCE).await?;
    ensure!(
        plain.process_views.is_empty(),
        "a nonretained commit invented a view"
    );
    let (mut tx, view) = store.open_process(&key, claim.epoch).await?;
    let expected = tx.revision();
    tx.retain_process(view.clone());
    // Neither registry event changes actor revision; both must survive capture.
    backend
        .process_registry()
        .set_external_ref(
            &process,
            lash_execution_contracts::process::ProcessExternalRef {
                backend: "receipt-law".into(),
                id: "external-job".into(),
                segment_ordinal: Some(1),
            },
        )
        .await
        .map_err(|error| LawBroken(error.to_string()))?;
    cancel(&backend, &process).await?;
    ensure!(
        store.begin(&key, claim.epoch).await?.revision() == expected,
        "external control moved actor authority"
    );
    let (_, frontier) = store
        .begin_process_retained(&key, claim.epoch, expected, 0)
        .await?;
    ensure!(
        frontier.cancel.is_some()
            && frontier.external_ref.is_some()
            && frontier.last_event_sequence > 0,
        "scalar frontier omitted external registry changes"
    );
    let driver = super::super::process::driver::Driver::default();
    tx.write(DomainWrite::Process(
        lash_durable::domain::ProcessWrite::Advance {
            process: process.clone(),
            expected_rev: 0,
            driver_json: driver.encode(),
        },
    ));
    let engine = LawEngine::new(0);
    let state = EngineState {
        format: engine.state_format(),
        bytes: b"receipt-state".to_vec(),
        pages: None,
        staged: Vec::new(),
        requirements: Default::default(),
    };
    super::super::process::state_graph::write_engine_state(
        &mut tx,
        &process,
        LAW_ENGINE_KIND,
        &state,
        None,
    )?;
    let receipt = store.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
    let (_, actual) = store.open_process(&key, claim.epoch).await?;
    ensure!(
        receipt.process_views == vec![actual.clone()],
        "receipt differs from stored postimage: {receipt:?} vs {actual:?}"
    );
    for views in [Vec::new(), vec![actual.clone(), actual.clone()]] {
        let mut invalid = receipt.clone();
        invalid.process_views = views;
        let projection = super::super::process::continuation::Continuation::loaded(actual.clone());
        ensure!(
            matches!(
                projection.acknowledge(&invalid, &key, false),
                Err(DurableError::ProcessPostimage { .. })
            ),
            "an incomplete/extra process receipt was adopted"
        );
    }
    let mut next = store.begin(&key, claim.epoch).await?;
    next.retain_process(actual);
    let output = ProcessOutcome::from_tool_output(ToolCallOutput::success(json!({})));
    super::super::process::record_terminal(&mut next, &process, &output)?;
    let receipt = store.commit(next, CommitLabel::PROCESS_TERMINAL).await?;
    let (_, actual) = store.open_process(&key, claim.epoch).await?;
    ensure!(
        receipt.process_views == vec![actual],
        "terminal receipt differs from stored postimage"
    );
    Ok(())
}
