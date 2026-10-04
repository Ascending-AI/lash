//! L03/L07: the Run consumes the real source seal. Resolution protects
//! finalization even when cancellation arrives inside a hook or presentation.
use super::*;
use crate::tool_dispatch::{
    BeforeCheckReply, RunCoordinator, SingletonAttempt, SingletonBodyOutcome, SingletonCapture,
    SingletonPreparedRequest, SingletonToolCall, SingletonToolHandlers,
};
use crate::tool_run::{
    AdmittedBinding, AfterCheckVerdict, AttributedVerdict, CallDecision, ExternalCancelPolicy,
    MaterialBundle, MaterialHolder, MaterialOwner, MaterialPayload, MaterialRole,
    PresentationBinding, RunEvent, SealOutcome, SegmentOrdinal, SourceAuthority, SourceDescriptor,
    SourceSeal, ToolDeclaration,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug)]
pub(super) enum Hold {
    AfterCheck,
    Presentation,
    None,
}

struct Probe {
    materials: Arc<dyn crate::store::ToolMaterialStore>,
    cancel: AtomicBool,
    executions: AtomicUsize,
    checks: AtomicUsize,
    presentations: AtomicUsize,
    source: tokio::sync::watch::Sender<Option<crate::AwaitEventKey>>,
    proceed: tokio::sync::watch::Sender<bool>,
    entered: tokio::sync::watch::Sender<bool>,
    released: tokio::sync::watch::Sender<bool>,
    held: Hold,
}
impl Probe {
    async fn hold(&self) {
        self.entered.send_replace(true);
        let mut released = self.released.subscribe();
        released
            .wait_for(|value| *value)
            .await
            .unwrap_or_else(|error| panic!("the held final must be released: {error}"));
    }
}
#[async_trait::async_trait]
impl SingletonToolHandlers for Probe {
    fn tool_material_store(&self) -> Option<&dyn crate::store::ToolMaterialStore> {
        Some(&*self.materials)
    }
    fn run_cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String> {
        Ok(call.arguments.clone())
    }
    async fn before_checks(
        &self,
        _: &SingletonToolCall,
        _: &SingletonPreparedRequest,
    ) -> Vec<AttributedVerdict<BeforeCheckReply>> {
        Vec::new()
    }
    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        let source = attempt
            .completion_key
            .unwrap_or_else(|| panic!("the Run must arm its source before the body"))
            .clone();
        self.source.send_replace(Some(source.clone()));
        Ok(SingletonBodyOutcome::Deferred { source })
    }
    async fn after_checks(
        &self,
        _: &crate::ToolCallId,
        _: &SingletonCapture,
    ) -> Vec<AttributedVerdict<AfterCheckVerdict>> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        if matches!(self.held, Hold::AfterCheck) {
            self.hold().await;
        }
        Vec::new()
    }
    async fn realize_declarations(
        &self,
        _: &crate::ToolCallId,
        _: &[crate::ToolIntentKind],
    ) -> Result<(), String> {
        Ok(())
    }
    fn emit_stream(&self, _: &crate::ToolCallId, _: &crate::runtime::AttemptStream) {}
    async fn launch_start(
        &self,
        _: &crate::tool_dispatch::DeclaredStartObligation,
    ) -> Result<crate::ProcessId, String> {
        Err("no start declared".to_owned())
    }
    async fn discharge_start(
        &self,
        _: &crate::tool_dispatch::DeclaredStartObligation,
        _: &crate::ProcessId,
        _: bool,
    ) -> Result<(), String> {
        Err("no start declared".to_owned())
    }
    async fn present(
        &self,
        _: &crate::ToolCallId,
        _: &SingletonCapture,
    ) -> Result<String, crate::tool_dispatch::SingletonPresentationError> {
        self.presentations.fetch_add(1, Ordering::SeqCst);
        if matches!(self.held, Hold::Presentation) {
            self.hold().await;
        }
        Ok("real external result".to_owned())
    }
}

/// Both resolve-first orderings drain exactly one final through the Run.
pub async fn a_deferred_childs_commit_point_is_its_resolution(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    for held in [Hold::AfterCheck, Hold::Presentation] {
        deferred_run(fixture, &format!("{prefix}-{held:?}"), false, held).await;
    }
}

pub(super) async fn deferred_run(
    fixture: &ToolChildLawFixture,
    prefix: &str,
    cancel_first: bool,
    held: Hold,
) {
    let scope = crate::ExecutionScope::turn(
        crate::SessionId::fixture(format!("{prefix}-source")),
        crate::TurnId::fixture("run"),
    );
    let owner = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .unwrap_or_else(|error| panic!("the source law owns a turn: {error}"));
    let revision =
        crate::plugin::PluginRevision::new("source-law", crate::plugin::BehaviorRevision::ONE);
    let callback = crate::store::plugin_writers::PluginCallbackIdentity {
        owner: revision.clone(),
        key: "tool:source".to_owned(),
    };
    let call = Arc::new(SingletonToolCall {
        owner: owner.clone(),
        segment: SegmentOrdinal(0),
        call_id: crate::ToolCallId::fixture(prefix),
        tool_name: "source".to_owned(),
        arguments: serde_json::Value::Null,
        declaration: ToolDeclaration::deferring(),
        binding: AdmittedBinding {
            executable: callback.clone(),
            preparation: callback,
            presentation: PresentationBinding {
                presenter: None,
                steps: Vec::new(),
            },
        },
        available: vec![revision.clone()],
        cancel: ExternalCancelPolicy::Ignore,
        environment: None,
    });
    let probe = Arc::new(Probe {
        materials: Arc::clone(&fixture.source_materials),
        cancel: AtomicBool::new(false),
        executions: AtomicUsize::new(0),
        checks: AtomicUsize::new(0),
        presentations: AtomicUsize::new(0),
        source: tokio::sync::watch::channel(None).0,
        proceed: tokio::sync::watch::channel(false).0,
        entered: tokio::sync::watch::channel(false).0,
        released: tokio::sync::watch::channel(false).0,
        held,
    });
    let records = Arc::new(std::sync::Mutex::new(None));
    let attempt: crate::ConformanceTurnAttempt = Arc::new({
        let call = Arc::clone(&call);
        let probe = Arc::clone(&probe);
        let records = Arc::clone(&records);
        move |scoped| {
            let call = Arc::clone(&call);
            let probe = Arc::clone(&probe);
            let records = Arc::clone(&records);
            Box::pin(async move {
                let mut run = RunCoordinator::open(
                    &scoped,
                    call.owner.clone(),
                    call.segment,
                    call.available.clone(),
                );
                assert!(matches!(
                    run.decide(&call, &*probe)
                        .await
                        .unwrap_or_else(|error| panic!("the Run must record Deferred X: {error}")),
                    crate::tool_dispatch::DecidedCall::Deferred { .. }
                ));
                assert!(
                    run.records()
                        .iter()
                        .flat_map(|r| &r.events)
                        .all(|e| !matches!(
                            e,
                            RunEvent::Decided { .. } | RunEvent::Presented { .. }
                        ))
                );
                let mut proceed = probe.proceed.subscribe();
                proceed.wait_for(|p| *p).await.unwrap_or_else(|error| {
                    panic!("the source writer must release the Run: {error}")
                });
                run.await_deferred()
                    .await
                    .unwrap_or_else(|error| panic!("the Run must accept its source seal: {error}"));
                run.drain()
                    .await
                    .unwrap_or_else(|error| panic!("the accepted final must drain: {error}"));
                *records.lock_recover() = Some(run.into_records());
                crate::ConformanceTurnEnd::Settled
            })
        }
    });
    let write = async {
        let mut source = probe.source.subscribe();
        let source = source
            .wait_for(Option::is_some)
            .await
            .unwrap_or_else(|error| panic!("the body must publish its source: {error}"))
            .clone()
            .unwrap_or_else(|| panic!("the published source must exist"));
        let descriptor = SourceDescriptor {
            source: source.clone(),
            call_id: call.call_id.clone(),
            owner,
            resolver: revision,
            authority: SourceAuthority::ExternalCompletion,
            cancel: call.cancel,
        };
        let capture = SingletonCapture::Done {
            output: "real external result".to_owned(),
            commands: Vec::new(),
            intents: Vec::new(),
            stream: Default::default(),
            start: None,
        };
        let bundle = MaterialBundle::of(vec![MaterialPayload::new(
            MaterialOwner::Source {
                source: source.clone(),
            },
            MaterialRole::AttemptOutput,
            None,
            serde_json::to_string(&capture)
                .unwrap_or_else(|error| panic!("the final capture must encode: {error}")),
        )])
        .unwrap_or_else(|error| panic!("the source result bundle must validate: {error}"))
        .unwrap_or_else(|| panic!("the source result bundle must contain its result"));
        let retained = fixture
            .source_materials
            .retain_material(&MaterialHolder::Source { source }, &bundle)
            .await
            .unwrap_or_else(|error| panic!("the source result must be retained: {error}"));
        let seal = SourceSeal::Resolved {
            result: Box::new(retained.references[0].clone()),
        };
        if cancel_first {
            probe.cancel.store(true, Ordering::SeqCst);
            probe.proceed.send_replace(true);
            let runner_done = async {
                while records.lock_recover().is_none() {
                    tokio::time::sleep(POLL).await;
                }
            };
            tokio::time::timeout(SETTLE_BUDGET, runner_done)
                .await
                .unwrap_or_else(|error| panic!("the cancelled Run must finish: {error}"));
            for _ in 0..2 {
                assert_eq!(
                    (fixture.seal_source)(descriptor.clone(), seal.clone()).await,
                    SealOutcome::AlreadySealed {
                        seal: SourceSeal::Cancelled
                    },
                    "late writes never revive the cancelled call"
                );
            }
        } else {
            assert_eq!(
                (fixture.seal_source)(descriptor, seal.clone()).await,
                SealOutcome::Sealed { seal }
            );
            probe.proceed.send_replace(true);
            let mut entered = probe.entered.subscribe();
            entered.wait_for(|e| *e).await.unwrap_or_else(|error| {
                panic!("the resolved final must reach its protected phase: {error}")
            });
            probe.cancel.store(true, Ordering::SeqCst);
            probe.released.send_replace(true);
        }
    };
    tokio::time::timeout(SETTLE_BUDGET, async {
        tokio::join!(
            fixture.turn_runner.run_turn(crate::admit(scope), attempt),
            write
        );
    })
    .await
    .unwrap_or_else(|error| panic!("the source law must settle: {error}"));
    let records = records
        .lock_recover()
        .clone()
        .unwrap_or_else(|| panic!("the Run must publish its records"));
    let decisions: Vec<_> = records
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match e {
            RunEvent::Decided { rank, decision, .. } => Some((*rank, decision)),
            _ => None,
        })
        .collect();
    assert_eq!(decisions.len(), 1, "one real terminal takes one rank");
    assert_eq!(decisions[0].0, 1);
    assert_eq!(
        matches!(decisions[0].1, CallDecision::Cancelled),
        cancel_first
    );
    assert_eq!(
        matches!(decisions[0].1, CallDecision::Final { .. }),
        !cancel_first
    );
    assert_eq!(probe.executions.load(Ordering::SeqCst), 1);
    assert_eq!(
        probe.checks.load(Ordering::SeqCst),
        usize::from(!cancel_first)
    );
    assert_eq!(
        probe.presentations.load(Ordering::SeqCst),
        usize::from(!cancel_first)
    );
    assert_eq!(
        records
            .iter()
            .flat_map(|r| &r.events)
            .filter(|e| matches!(e, RunEvent::Presented { .. }))
            .count(),
        1
    );
}
