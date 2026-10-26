use super::*;

/// FIG-7255: admitted tool identities survive recovery reads, the pending
/// trace projection and completed cell reports. Use committed SQLite facts
/// directly so this law needs no model, VM cell or approval park.
#[tokio::test]
async fn legal_tool_identities_remain_visible_in_call_ledger_projections() {
    use lash_core_execution::runtime::actor::round::{self, ExecutionDraft};
    use lash_core_store::effect_opener::EffectOpener;
    use lash_core_store::tool_run::{Material, MaterialOwner, MaterialRole, SettledOutput};
    use lash_durable::domain::{OwnerKey, RunSeq};
    use lash_trace::{DurableTraceScope, TraceScopeId, TraceScopeOwner, TraceToolOwner};
    use lash_vocabulary::identity::TurnId;
    use {
        lash_kernel_value::tool_contract::ExecutionPolicy, lash_vocabulary::tool_contract::ToolId,
    };

    let backend = crate::testing::sqlite_recording_backend().await;
    let session = SessionId::fixture("call-observation-law");
    let run = TurnId::fixture("call-observation-run");
    let _store =
        crate::testing::runtime_helpers::recording_session_store(&backend, session.clone()).await;
    let actor = lash_durable::ActorKey::session(session.as_str()).unwrap();
    let mut mail = lash_durable::MailTx::new();
    mail.append(
        actor.clone(),
        lash_durable::MailKind::new("law.call-observations"),
        String::new(),
    );
    backend
        .durable()
        .commit_mail(mail, lash_durable::CommitLabel::MAIL_SESSION)
        .await
        .unwrap();
    let node = backend
        .durable()
        .register_node(&lash_durable::NodeSpec {
            node: lash_durable::NodeId::new("call-observation-node"),
            decodes: backend.formats().decodes(),
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let claimed = backend.durable().claim(&node, 1).await.unwrap();
    let cx = ActorContext::new(
        backend.clone(),
        actor,
        claimed[0].epoch,
        crate::AdmittedScope::turn(session.clone(), run.clone()),
        CancellationToken::new(),
        Arc::new(lash_durable::NoProbe),
    );
    let _claim = cx.session_claim_guard();
    let owner = OwnerKey::Turn(session.clone(), run.clone());
    let opener = EffectOpener::turn(session.clone(), run.clone());
    let answered = crate::ToolCallId::fixture("answered");
    let pending = crate::ToolCallId::fixture("pending");
    let untraced = crate::ToolCallId::fixture("untraced");
    let trace = DurableTraceScope::admit(
        TraceScopeId::admission(TraceScopeOwner::Tool {
            owner: TraceToolOwner::Turn {
                session_id: session.clone(),
                turn_id: run.clone(),
            },
            call_id: pending.as_str().to_owned(),
        }),
        None,
        Default::default(),
        0,
    );
    let request = Material::journal_local(
        MaterialOwner::Run {
            opener: opener.clone(),
        },
        MaterialRole::PreparedRequest,
        "{}".to_owned(),
    );
    let draft = |call, tool| {
        ExecutionDraft::new(
            lash_durable::DurableInstant(0),
            call,
            ToolId::new(tool),
            request.reference().clone(),
            ExecutionPolicy::Once,
            crate::ExecutionLimit {
                expires_at: u64::MAX,
                max_slice: std::time::Duration::from_secs(60),
            },
            None,
        )
    };
    let mut tx = cx.begin().await.unwrap();
    let members = round::admit(
        &mut tx,
        &owner,
        RunSeq(1),
        vec![
            draft(answered.clone(), "cell-host:customer"),
            draft(pending.clone(), "cell-host:approval").with_trace(Some(trace.clone())),
            draft(untraced.clone(), "ordinary"),
        ],
    )
    .unwrap();
    let output = crate::ToolCallOutput::success(serde_json::json!({"found": true}));
    let expected = crate::ToolCallRecord {
        call_id: answered.clone(),
        provider_call_id: None,
        tool: "tools.lookup".to_owned(),
        args: serde_json::json!({}),
        output: output.clone(),
    };
    let completed = round::CompletedCall {
        call_id: answered,
        provider_call_id: None,
        tool_name: expected.tool.clone(),
        args: expected.args.clone(),
        model_return: crate::ModelToolReturn::from_output(expected.tool.clone(), &output),
        output,
        control_origin: lash_sansio::ToolControlOrigin::OwnCall,
        intent_outcomes: Vec::new(),
        replay: None,
    };
    round::settle(
        &mut tx,
        &members[0],
        SettledOutput::Completed(round::completed_material(&opener, completed).unwrap()),
        Vec::new(),
        None,
    )
    .unwrap();
    cx.commit(tx, lash_durable::CommitLabel::ROUND_OUTCOME)
        .await
        .unwrap();

    let facts = backend.durable().run_facts(&owner).await.unwrap();
    let fold = round::fold(
        &facts,
        &round::PolicyView::new([]),
        &round::PinnedWaits::ledger(),
    )
    .unwrap();
    let traced = open_cell_call_drafts(&fold).collect::<Vec<_>>();
    assert_eq!(
        traced
            .iter()
            .map(|draft| draft.call().clone())
            .collect::<Vec<_>>()
            .as_slice(),
        std::slice::from_ref(&pending)
    );
    assert_eq!(
        traced[0].trace(),
        Some(&trace),
        "the pending call keeps its admitted trace"
    );
    let report = cell_calls_from_ledger(&facts, &fold, CellToolCalls::default()).unwrap();
    assert_eq!(
        report.records.as_slice(),
        std::slice::from_ref(&expected),
        "the report retains the legal prefixed call"
    );
    let mut decoder =
        lash_core_store::checkpoint_graph::GraphDecoder::new(&facts, facts.read_limits);
    assert_eq!(report.ledger.entries.len(), 1);
    assert_eq!(
        round::graph::CallBody::load(&mut decoder, report.ledger.entries[0])
            .unwrap()
            .record,
        expected
    );

    let (records, open) = crate::runtime::durable::services::RuntimeTurnServices::tool_call_bodies(
        &cx,
        &session,
        &run,
        Some(&run),
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        records,
        [expected],
        "recovery retains the answered prefixed call"
    );
    assert_eq!(
        open,
        [pending, untraced].into_iter().collect(),
        "recovery retains every admitted pending identity"
    );
}
