//! Retained call publications, keyed hydration and explicit expiry.
use super::*;

/// CALL-DIRECTORY: decisions precede answers; keyed hydration is independent
/// of retained history; conflicts roll back; only logical expiry retires calls.
pub async fn tool_call_publications_are_atomic_retained_and_keyed(
    tier: &dyn GraphTier,
    peer: &dyn DurableStore,
) -> LawOutcome {
    use crate::domain::tool_calls::ToolCallSettlement;
    use crate::domain::{
        CallOwner, ToolCallAnswer, ToolCallDecision, ToolCallSelection, ToolCallWrite,
    };
    use lash_core_store::tool_run::{AttemptOutcome, AvailableEvidence};
    use lash_protocol_types::ToolCallId;

    let owner = owned(tier, "call-directory").await?;
    let execution = owner.exec("calls").owner();
    let logical = crate::domain::tool_calls::logical_owner(&execution);
    let mut nodes = staging();
    let mut writes = Vec::new();
    let mut answers = Vec::new();
    let mut body_nodes = BTreeMap::new();
    let mut calls = Vec::new();
    let mut selected_nodes = BTreeSet::new();
    for member in 0..100 {
        let call = ToolCallId::fixture(&format!("directory-{member}"));
        let mut body = staging();
        let root = value(&mut body, &[&big(&format!("answer-{member}"))], &[]);
        let body = body.into_nodes();
        if member == 0 {
            selected_nodes.extend(body.keys().copied());
        }
        body_nodes.extend(body);
        writes.push(DomainWrite::ToolCall(ToolCallWrite::Admit {
            counted: true,
            logical: logical.clone(),
            execution: execution.clone(),
            run: RunSeq(0),
            member,
            call: call.clone(),
            cancellable: false,
        }));
        answers.push(DomainWrite::ToolCall(ToolCallWrite::Answer {
            owner: execution.clone(),
            call: call.clone(),
            answer: ToolCallAnswer {
                outcome: ToolCallSettlement::Read,
                body: Some(root),
            },
        }));
        calls.push((call, root));
    }
    writes.extend(answers);
    let pending = ToolCallId::fixture("directory-pending");
    writes.push(DomainWrite::ToolCall(ToolCallWrite::Admit {
        counted: true,
        logical: logical.clone(),
        execution: execution.clone(),
        run: RunSeq(1),
        member: 0,
        call: pending.clone(),
        cancellable: true,
    }));
    let control_owner = CallOwner::Operation(owner.session.clone(), "retained-control".into());
    let control_execution = owner.exec("control").owner();
    let control = ToolCallId::fixture("directory-control");
    let control_root = value(&mut nodes, &[&big("retained-control")], &[]);
    let complete = ToolCallWrite::Complete {
        owner: control_execution.clone(),
        call: control.clone(),
        fact: control_root,
        answer: ToolCallAnswer {
            outcome: ToolCallSettlement::Materialized(ToolCallDecision::Final),
            body: Some(control_root),
        },
    };
    writes.extend([
        DomainWrite::ToolCall(ToolCallWrite::Admit {
            counted: true,
            logical: control_owner.clone(),
            execution: control_execution.clone(),
            run: RunSeq(0),
            member: 0,
            call: control.clone(),
            cancellable: false,
        }),
        DomainWrite::ToolCall(complete.clone()),
    ]);
    let mut nodes = nodes.into_nodes();
    nodes.extend(body_nodes);
    owner_commit(tier, &owner, nodes, writes).await?;
    owner_commit(
        tier,
        &owner,
        BTreeMap::new(),
        vec![DomainWrite::ToolCall(complete.clone())],
    )
    .await?;
    let mut malformed = complete.clone();
    if let ToolCallWrite::Complete { answer, .. } = &mut malformed {
        answer.body = None;
    }
    ensure!(
        owner_commit(
            tier,
            &owner,
            BTreeMap::new(),
            vec![DomainWrite::ToolCall(malformed)]
        )
        .await
        .is_err(),
        "a materialized settlement committed without its canonical body"
    );
    let mut conflicting = complete;
    if let ToolCallWrite::Complete { answer, .. } = &mut conflicting {
        answer.body = Some(calls[0].1);
    }
    ensure!(
        owner_commit(
            tier,
            &owner,
            BTreeMap::new(),
            vec![DomainWrite::ToolCall(conflicting)]
        )
        .await
        .is_err(),
        "a combined final overwrote its immutable answer"
    );
    let selected = peer
        .tool_calls(
            &logical,
            &ToolCallSelection::Calls(vec![calls[0].0.clone(), ToolCallId::fixture("unknown")]),
        )
        .await?;
    ensure!(
        selected.rows.len() == 1,
        "a keyed read included unrelated calls"
    );
    ensure!(
        selected
            .graph
            .nodes
            .keys()
            .copied()
            .collect::<BTreeSet<_>>()
            == selected_nodes,
        "a keyed read hydrated unrelated bodies from 100-call history"
    );
    let mut after = None;
    let mut listed = Vec::new();
    loop {
        let page = peer
            .tool_calls(&logical, &ToolCallSelection::List { after, limit: 17 })
            .await?;
        let done = page.rows.len() < 17;
        after = page.rows.last().map(|row| crate::domain::ToolCallCursor {
            sequence: row.sequence,
            call: row.call.clone(),
        });
        listed.extend(page.rows.into_iter().map(|row| row.call));
        if done {
            break;
        }
    }
    ensure!(
        listed
            == calls
                .iter()
                .map(|(id, _)| id.clone())
                .chain([pending.clone()])
                .collect::<Vec<_>>(),
        "paged listing skipped or repeated an admission"
    );
    let hydrated = peer
        .hydrate(
            RootOwner::ToolCall {
                owner: execution.clone(),
                call: calls[0].0.clone(),
                part: crate::domain::tool_calls::ToolCallRoot::Answer,
            },
            Some(calls[0].1),
            DecodeLimits::within_total(1 << 28),
        )
        .await?;
    ensure!(
        hydrated.root == calls[0].1,
        "execution-addressed root resolved another logical owner"
    );
    let pages = peer
        .pages()
        .ok_or_else(|| LawBroken("the call store has no protected page reader".into()))?;
    let lease = pages
        .acquire_root_lease(crate::domain::pages::RootLeaseRequest {
            owner: RootOwner::ToolCall {
                owner: execution.clone(),
                call: calls[0].0.clone(),
                part: crate::domain::tool_calls::ToolCallRoot::Answer,
            },
            expected: calls[0].1,
            fence: crate::domain::pages::LeaseFence::Reader,
            ttl_millis: 600_000,
        })
        .await?;
    let read_page = peer
        .tool_calls(
            &logical,
            &ToolCallSelection::SavedReads {
                after: None,
                limit: 1,
            },
        )
        .await?;
    ensure!(
        read_page.rows.len() == 1
            && read_page.rows[0].call == calls[0].0
            && read_page
                .graph
                .nodes
                .keys()
                .copied()
                .collect::<BTreeSet<_>>()
                == selected_nodes,
        "a saved-Read page hydrated unrelated history"
    );
    let lookup = || ToolCallSelection::Calls(vec![pending.clone()]);
    let before = peer.tool_calls(&logical, &lookup()).await?;
    ensure!(
        before.rows.len() == 1
            && before.rows[0].answer.is_none()
            && before.rows[0].decision.is_none()
            && before.graph.nodes.is_empty(),
        "admission did not publish exactly Pending"
    );

    let duplicate_probe = ToolCallId::fixture("directory-after-duplicate");
    owner_commit(
        tier,
        &owner,
        BTreeMap::new(),
        vec![
            DomainWrite::ToolCall(ToolCallWrite::Admit {
                counted: true,
                logical: logical.clone(),
                execution: execution.clone(),
                run: RunSeq(0),
                member: 0,
                call: calls[0].0.clone(),
                cancellable: false,
            }),
            DomainWrite::ToolCall(ToolCallWrite::Admit {
                counted: true,
                logical: logical.clone(),
                execution: execution.clone(),
                run: RunSeq(0),
                member: 100,
                call: duplicate_probe.clone(),
                cancellable: false,
            }),
        ],
    )
    .await?;
    let after_duplicate = peer
        .tool_calls(&logical, &ToolCallSelection::Calls(vec![duplicate_probe]))
        .await?;
    ensure!(
        after_duplicate.rows[0].sequence == before.rows[0].sequence + 1,
        "a repeated admission allocated another admission-order position"
    );

    let mut decision = staging();
    let decision_root = value(&mut decision, &[&big("cancel-decision")], &[]);
    owner_commit(
        tier,
        &owner,
        decision.into_nodes(),
        vec![DomainWrite::ToolCall(ToolCallWrite::Decide {
            owner: execution.clone(),
            call: pending.clone(),
            decision: ToolCallDecision::Cancel,
            fact: decision_root,
        })],
    )
    .await?;
    let decided = peer.tool_calls(&logical, &lookup()).await?;
    ensure!(
        decided.rows[0].decision == Some(ToolCallDecision::Cancel)
            && decided.rows[0].answer.is_none()
            && decided.graph.nodes.is_empty(),
        "a decision exposed an answer before discharge"
    );
    let hydrated = peer
        .hydrate(
            RootOwner::ToolCall {
                owner: execution.clone(),
                call: pending.clone(),
                part: crate::domain::tool_calls::ToolCallRoot::Decision,
            },
            Some(decision_root),
            DecodeLimits::within_total(1 << 28),
        )
        .await?;
    ensure!(
        hydrated.root == decision_root,
        "the protected decision root was not retained"
    );
    ensure!(
        owner_commit(
            tier,
            &owner,
            BTreeMap::new(),
            vec![DomainWrite::ToolCall(ToolCallWrite::Decide {
                owner: execution.clone(),
                call: pending.clone(),
                decision: ToolCallDecision::Final,
                fact: decision_root,
            })]
        )
        .await
        .is_err(),
        "a conflicting final replaced the cancel decision"
    );
    let late_success = DomainWrite::ToolCall(ToolCallWrite::Answer {
        owner: execution.clone(),
        call: pending.clone(),
        answer: ToolCallAnswer {
            outcome: ToolCallSettlement::Read,
            body: Some(calls[0].1),
        },
    });
    ensure!(
        owner_commit(tier, &owner, BTreeMap::new(), vec![late_success.clone()])
            .await
            .is_err(),
        "late success replaced a cancel winner"
    );
    let cancelled = ToolCallAnswer {
        outcome: ToolCallSettlement::execution(AttemptOutcome::Cancelled {
            cancellation: lash_protocol_types::ToolCancellation::runtime("caller cancelled"),
            evidence: AvailableEvidence::default(),
        }),
        body: None,
    };
    owner_commit(
        tier,
        &owner,
        BTreeMap::new(),
        vec![DomainWrite::ToolCall(ToolCallWrite::Answer {
            owner: execution.clone(),
            call: pending.clone(),
            answer: cancelled.clone(),
        })],
    )
    .await?;
    ensure!(
        peer.tool_calls(&logical, &lookup()).await?.rows[0].answer == Some(cancelled),
        "cancel discharge did not publish its typed terminal"
    );
    ensure!(
        owner_commit(tier, &owner, BTreeMap::new(), vec![late_success.clone()])
            .await
            .is_err(),
        "a late answer overwrote settlement"
    );

    let read_page = peer
        .tool_calls(
            &logical,
            &ToolCallSelection::SavedReads {
                after: None,
                limit: 128,
            },
        )
        .await?;
    ensure!(
        read_page.rows.len() == 100 && read_page.rows.iter().all(|row| row.call != pending),
        "saved-Read listing included recorded terminals"
    );
    let rolled_back = ToolCallId::fixture("rolled-back-call");
    let admit = |call, run| {
        DomainWrite::ToolCall(ToolCallWrite::Admit {
            counted: true,
            logical: logical.clone(),
            execution: execution.clone(),
            run: RunSeq(run),
            member: 0,
            call,
            cancellable: false,
        })
    };
    ensure!(
        owner_commit(
            tier,
            &owner,
            BTreeMap::new(),
            vec![admit(rolled_back.clone(), 4), admit(calls[0].0.clone(), 5)]
        )
        .await
        .is_err(),
        "conflicting admission was accepted"
    );
    ensure!(
        peer.tool_calls(&logical, &ToolCallSelection::Calls(vec![rolled_back]))
            .await?
            .rows
            .is_empty(),
        "a refused batch left an earlier admission"
    );
    owner_commit(
        tier,
        &owner,
        BTreeMap::new(),
        vec![DomainWrite::RunFact(RunFactWrite::Prune {
            owner: execution.clone(),
            before: RunSeq(u64::from(u32::MAX)),
        })],
    )
    .await?;
    ensure!(
        peer.tool_calls(&logical, &lookup()).await?.rows.len() == 1,
        "journal compaction expired call history"
    );
    let racing_selection = ToolCallSelection::Calls(vec![calls[0].0.clone()]);
    let (read, expired) = tokio::join!(
        peer.tool_calls(&logical, &racing_selection),
        owner_commit(
            tier,
            &owner,
            BTreeMap::new(),
            vec![DomainWrite::ToolCall(ToolCallWrite::Expire {
                owner: logical.clone()
            })]
        )
    );
    expired?;
    let read = read?;
    ensure!(
        read.rows.is_empty() && read.graph.nodes.is_empty()
            || read.rows.len() == 1
                && read.graph.nodes.keys().copied().collect::<BTreeSet<_>>() == selected_nodes,
        "expiry raced a partial body read"
    );
    ensure!(
        peer.tool_calls(&logical, &lookup()).await?.rows.is_empty(),
        "expired calls still read"
    );
    ensure!(
        owner_commit(tier, &owner, BTreeMap::new(), vec![late_success])
            .await
            .is_err(),
        "a late outcome resurrected expired history"
    );
    ensure!(
        peer.tool_calls(&control_owner, &ToolCallSelection::Calls(vec![control]))
            .await?
            .rows
            .len()
            == 1
            && present(tier, &control_root).await,
        "expiry removed the retained neighboring owner"
    );
    ensure!(
        present(tier, &calls[0].1).await,
        "expiry reclaimed a leased archival body"
    );
    ensure!(
        pages.read_pages(&lease, &[calls[0].1]).await?.len() == 1,
        "a live reader lease lost its page"
    );
    pages.release_root_lease(lease.id).await?;
    tier.retire().await?;
    ensure!(
        !present(tier, &calls[0].1).await,
        "lease release kept an expired archival body"
    );
    exact(tier, "call expiry and retained control").await
}
