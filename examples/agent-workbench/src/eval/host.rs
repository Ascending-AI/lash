//! A bounded Standard workbench composition with no host-affecting tools.
//! The only recovery control is killing the process at an existing trace seam.
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use lash::tracing::{TraceEvent, TraceRecord, TraceSink, TraceSinkError};

use super::manifest::Scenario;
use super::{Evidence, Job, write_json};

struct CutTrace {
    file: crate::trace_file::JsonlFileSink,
    cut_turn: Option<lash::TurnId>,
    path: std::path::PathBuf,
}

impl TraceSink for CutTrace {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        self.file.append(record)?;
        if self.cut_turn.is_some()
            && record.context.turn_id == self.cut_turn
            && matches!(record.event, TraceEvent::LlmCallStarted { .. })
        {
            // This is a barrier at an emitted Lash fact, not another event log.
            // Parent waits for this typed record, SIGKILLs and reaps us. Blocking
            // here keeps fast providers from settling before the kill lands.
            std::fs::OpenOptions::new()
                .write(true)
                .open(&self.path)
                .and_then(|file| file.sync_all())
                .map_err(|source| TraceSinkError::Write {
                    path: self.path.clone(),
                    source,
                })?;
            loop {
                std::thread::park();
            }
        }
        Ok(())
    }
}

pub(super) fn trace(path: &Path) -> Result<Vec<TraceRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(path)?;
    // A killed writer can leave one incomplete final line. It is retained in
    // trace.jsonl and reported as missing coverage, never silently repaired.
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).context("decode typed trace record"))
        .collect()
}

pub(super) fn cut_seen(records: &[TraceRecord], case: &super::Case) -> bool {
    let turn = input_id(case.inputs.len() - 1);
    records.iter().any(|record| {
        record.context.turn_id.as_ref() == Some(&turn)
            && matches!(record.event, TraceEvent::LlmCallStarted { .. })
    })
}

#[expect(
    clippy::expect_used,
    reason = "the formatted eval identity is always nonempty"
)]
pub(super) fn input_id(index: usize) -> lash::TurnId {
    lash::TurnId::parse(format!("eval-input-{index}")).expect("nonempty eval identity")
}

pub(super) fn provider(fixture: bool) -> Result<lash::provider::ProviderHandle> {
    #[cfg(test)]
    if fixture {
        return Ok(super::tests::provider());
    }
    ensure!(!fixture, "scripted fixture is test-only");
    let key = std::env::var(crate::OPENROUTER_API_KEY_ENV)
        .context("OPENROUTER_API_KEY required for operator evals")?;
    ensure!(!key.trim().is_empty(), "OPENROUTER_API_KEY is empty");
    let url = std::env::var("AGENT_WORKBENCH_PROVIDER_URL")
        .unwrap_or_else(|_| lash::openai::OPENROUTER_BASE_URL.into());
    Ok(lash::provider::ProviderHandle::new(
        lash::openai::OpenAiCompatibleProvider::new(key, url)
            .with_compat(lash::openai::OpenAiCompat::openrouter())
            .into_components(),
    ))
}

pub(super) async fn worker(job_path: &Path, phase: &str, fixture: bool) -> Result<()> {
    ensure!(
        phase == "initial" || phase == "resume",
        "unknown worker phase"
    );
    let job: Job = serde_json::from_slice(&std::fs::read(job_path)?)?;
    let stores = crate::WorkbenchStores::open_sqlite(&job.directory).await?;
    let backend = lash::durable::DurableBackendBuilder::new(stores.stores).build()?;
    let provider = provider(fixture)?;
    let profile = lash::RegisteredLlmProfile::new(
        lash::LlmProfileMetadata::builder(&job.model.model)
            .cache_retention(lash::provider::CacheRetention::Short)
            .context_window_tokens(200_000)
            .build()?,
        provider.clone(),
    );
    let trace_path = job.directory.join("trace.jsonl");
    let cut = phase == "initial" && job.case.scenario == Scenario::KillRestart;
    let sink = CutTrace {
        file: crate::trace_file::JsonlFileSink::new(&trace_path),
        cut_turn: cut.then(|| input_id(job.case.inputs.len() - 1)),
        path: trace_path.clone(),
    };
    let core = lash::LashCore::standard_builder(backend)
        .llm_profiles(Arc::new(
            lash::LlmProfileRegistry::new().register(job.model.model.as_str(), profile)?,
        ))
        .trace_sink(Arc::new(sink))
        .trace_detail(lash::tracing::TraceDetailPolicy::extended_models())
        .content_policy(lash::tracing::ContentPolicy::captured())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("workbench-eval"),
            lash::persistence::LeaseIncarnationId::new(uuid::Uuid::new_v4().to_string()),
        ))?;
    let id = lash::SessionId::parse("workbench-eval")?;
    let session = if phase == "initial" {
        core.session(id)
            .create(lash::SessionCreation::root(
                lash::SessionSpec::new(
                    job.model.model.clone(),
                    lash::TurnBudget::bounded(8),
                    lash::MaxToolCalls::new(1),
                )
                .no_progress_budget(lash::NoProgressBudget::bounded(2)),
            ))
            .await?
    } else {
        core.session(id).durable().await?
    };
    let result: Result<()> = async {
        if phase == "initial" {
            for (index, input) in job.case.inputs.iter().enumerate() {
                let output = session
                    .send(lash::TurnInput::text(input))
                    .id(input_id(index))
                    .output()
                    .await?;
                ensure!(output.is_success(), "input {index} failed: {output:?}");
            }
        } else {
            let output = session
                .attach_id(input_id(job.case.inputs.len() - 1))
                .output()
                .await?;
            ensure!(output.is_success(), "resumed input failed: {output:?}");
        }
        Ok(())
    }
    .await;
    // Export even a refused or parked run's last settled view. No host rows
    // substitute for the messages and snapshot Lash actually persisted.
    if let Some(view) = session.read().await? {
        core.flush_trace_sink()?;
        let evidence = Evidence {
            messages: view.messages().to_vec(),
            turn_index: view.turn_index(),
            settled_view: serde_json::to_value(view.to_snapshot())?,
            trace: trace(&trace_path)?,
        };
        write_json(&job.directory.join("evidence.json"), &evidence)?;
    }
    core.shutdown().await?;
    provider.close().await?;
    result
}

pub(super) fn transcript(evidence: &Evidence) -> String {
    let mut text = String::new();
    for message in &evidence.messages {
        let parts = message
            .parts
            .iter()
            .map(|part| part.content())
            .collect::<Vec<_>>()
            .join("\n");
        text.push_str(&format!("{:?}: {parts}\n\n", message.role));
    }
    text
}

pub(super) fn archive(job: &Job, evidence: &Evidence) -> Result<()> {
    std::fs::write(job.directory.join("transcript.txt"), transcript(evidence))?;
    write_json(
        &job.directory.join("settled-view.json"),
        &evidence.settled_view,
    )?;
    let mut file = std::fs::File::create(job.directory.join("trace-slice.jsonl"))?;
    for record in &evidence.trace {
        writeln!(file, "{}", serde_json::to_string(record)?)?;
    }
    Ok(())
}
