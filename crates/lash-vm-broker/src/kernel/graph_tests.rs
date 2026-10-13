#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;

use lash_core_store::checkpoint_graph::{
    DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphDecoder, GraphRefusal,
    GraphStaging, NodeId, parse_envelope,
};
use lash_core_store::effect_opener::EffectOpener;
use lash_kernel_doc::{Site, TaskIdentity, Unit};
use lash_sansio::{ExecutionPolicy, ToolCallId, ToolId};

use super::*;
use crate::kernel::value::datum_to_json;

fn staging() -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
}

/// What an immediate collection leaves: only the nodes `root` reaches.
fn collected(staging: &GraphStaging, root: NodeId) -> BTreeMap<NodeId, EncodedNode> {
    let mut kept = BTreeMap::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        if kept.contains_key(&id) {
            continue;
        }
        let node = staging
            .get(&id)
            .expect("a reachable node is staged")
            .clone();
        pending.extend(
            parse_envelope(&node.bytes, Some(id))
                .unwrap()
                .children
                .iter(),
        );
        kept.insert(id, node);
    }
    kept
}

/// A tool takes its one input record as JSON.
struct OneRecord(&'static str);

impl ArgumentProjection for OneRecord {
    fn id(&self) -> &str {
        self.0
    }

    fn project(&self, args: &[Datum]) -> Result<serde_json::Value, String> {
        let [input] = args else {
            return Err("one input record".to_owned());
        };
        let text = datum_to_json(input).map_err(|error| error.to_string())?;
        serde_json::from_str(&text).map_err(|error| error.to_string())
    }
}

impl Projections for OneRecord {
    fn project(&self, projection: &str, args: &[Datum]) -> Result<serde_json::Value, String> {
        if projection != self.0 {
            return Err(format!("no projection `{projection}`"));
        }
        ArgumentProjection::project(self, args)
    }
}

fn identity(occurrence: u64) -> EffectIdentity {
    EffectIdentity {
        task: TaskIdentity::Main,
        site: Site {
            unit: Unit::Main,
            path: vec![1, 2],
        },
        occurrence,
        loops: Vec::new(),
    }
}

fn exec() -> ExecKey {
    ExecKey::parse("c/session/turn/cell").unwrap()
}

fn call(id: &str, args: serde_json::Value) -> CellCall {
    CellCall {
        id: ToolCallId::fixture(id),
        tool_id: ToolId::new("read"),
        args,
        execution_grant: None,
        recorded_binding: None,
        issuing_language_node_id: Some("main/1.2".to_owned()),
    }
}

fn guest_args() -> Vec<Datum> {
    vec![Datum::Record(vec![
        ("path".to_owned(), Datum::Text("x".repeat(600))),
        ("lines".to_owned(), Datum::Int(3.into())),
    ])]
}

fn pin(call: &CellCall, request: NodeId) -> PinnedMember {
    PinnedMember {
        call: call.id.clone(),
        tool: "read".to_owned(),
        request: request_material(&EffectOpener::turn("session", "turn"), request),
        policy: ExecutionPolicy::Once,
        limit_expires_at_ms: 90,
        limit_max_slice_ms: 30,
        wait_id: None,
        trace: None,
        declaration: None,
        binding: None,
    }
}

/// PC-ADMISSION, the host half (FIG-5887 §3, §11): a call's request is
/// frozen once from the guest's own arguments, and only when the projection
/// makes exactly the call's arguments of them. Every effect the saved state
/// names, a call, a sleep, a refusal and a released call, resolves through
/// the membership to exactly one admission of the same execution after an
/// immediate collection, with its request; staging the same admissions
/// again on a retry makes the same nodes; and an effect the membership does
/// not name, another execution, another projection or a pin that names
/// another request is refused.
#[test]
fn every_saved_effect_resolves_to_its_one_frozen_admission() {
    let projection = OneRecord("cell-args/v1");
    let projected = ArgumentProjection::project(&projection, &guest_args()).unwrap();
    let lossy = FrozenRequest::tool(
        call("lossy", serde_json::json!({"path": "x"})),
        guest_args(),
        &projection,
    );
    assert_eq!(lossy, Err(FreezeRefusal::Lossy));

    let stage_all = |staging: &mut GraphStaging| {
        let read = call("read", projected.clone());
        let released = call("released", projected.clone());
        let mut entries = Vec::new();
        let mut admissions = Vec::new();
        let requests = [
            (
                0,
                FrozenRequest::tool(read.clone(), guest_args(), &projection).unwrap(),
                MemberStatus::Pending,
            ),
            (
                1,
                FrozenRequest::Sleep {
                    duration: std::time::Duration::from_millis(50),
                    until_ms: 1_050,
                },
                MemberStatus::Pending,
            ),
            (
                2,
                FrozenRequest::Refused {
                    error: ErrorDatum {
                        kind: "unknown_effect".to_owned(),
                        message: "no such effect".to_owned(),
                        data: Datum::Null,
                    },
                    args: Vec::new(),
                },
                MemberStatus::Pending,
            ),
            (
                3,
                FrozenRequest::tool(released.clone(), guest_args(), &projection).unwrap(),
                MemberStatus::Released,
            ),
        ];
        for (occurrence, request, status) in requests {
            let node = request.stage(staging).unwrap();
            let disposition = match &request {
                FrozenRequest::Tool { call, .. } => Disposition::Execution {
                    operation: OperationId {
                        run: 0,
                        ordinal: 1 + occurrence,
                    },
                    pin: Box::new(pin(call, node)),
                },
                FrozenRequest::Sleep { .. } => Disposition::Sleep,
                FrozenRequest::Refused { .. } => Disposition::Refused,
            };
            let admission = Admission {
                exec: exec(),
                effect: identity(occurrence),
                wait: occurrence,
                effect_name: Some(EffectName::new("read").unwrap()),
                request: node,
                disposition,
            };
            let admitted = admission.stage(staging).unwrap();
            admissions.push((admission, request));
            entries.push(MemberEntry {
                effect: identity(occurrence),
                wait: occurrence,
                admission: admitted,
                status,
            });
        }
        let membership = EffectMembership::new(1, entries).unwrap();
        (membership.stage(staging).unwrap(), admissions)
    };
    let mut staging = staging();
    let (root, admissions) = stage_all(&mut staging);
    let (retried, _) = stage_all(&mut self::staging());
    assert_eq!(retried, root, "a retry stages the same nodes");

    let kept = collected(&staging, root);
    let decoder = || GraphDecoder::new(&kept, DecodeLimits::within_total(1 << 24));
    for (admission, request) in &admissions {
        let resolved = resolve_admission(
            &mut decoder(),
            root,
            &exec(),
            &admission.effect,
            &projection,
        )
        .unwrap();
        assert_eq!(&resolved.admission, admission);
        assert_eq!(&resolved.request, request);
    }
    assert_eq!(
        resolve_admission(&mut decoder(), root, &exec(), &identity(9), &projection),
        Err(GraphRefusal::AdmissionMissing(Box::new(identity(9))))
    );
    let other = ExecKey::parse("c/session/turn/other").unwrap();
    assert!(matches!(
        resolve_admission(&mut decoder(), root, &other, &identity(0), &projection),
        Err(GraphRefusal::AdmissionMismatch(_))
    ));
    assert!(matches!(
        resolve_admission(
            &mut decoder(),
            root,
            &exec(),
            &identity(0),
            &OneRecord("cell-args/v2")
        ),
        Err(GraphRefusal::InvalidBody { .. })
    ));

    // A pin whose request material names another request is refused.
    let (mut forged, request) = admissions[0].clone();
    let other_request = FrozenRequest::Sleep {
        duration: std::time::Duration::from_millis(1),
        until_ms: 1,
    }
    .stage(&mut staging)
    .unwrap();
    if let (
        Disposition::Execution {
            pin: forged_pin, ..
        },
        FrozenRequest::Tool { call, .. },
    ) = (&mut forged.disposition, &request)
    {
        **forged_pin = pin(call, other_request);
    }
    let node = forged.stage(&mut staging).unwrap();
    let mut decoder = GraphDecoder::new(&staging, DecodeLimits::within_total(1 << 24));
    assert!(matches!(
        Admission::load(&mut decoder, node, &projection),
        Err(GraphRefusal::AdmissionMismatch(_))
    ));

    // Its row is keyed by the identity's canonical encoding.
    let lash_durable::DomainWrite::EffectAdmission(EffectAdmissionWrite::Put {
        effect,
        member,
        ..
    }) = admissions[0].0.write(node)
    else {
        panic!("an admission row");
    };
    assert_eq!(effect, effect_key(&identity(0)));
    assert_eq!(member.map(|member| member.member), Some(1));
}

/// A park's membership names each wait the ledger stands on by its
/// admission; a withdrawn execution stays named, released, until it
/// settles; an admission no wait or released execution needs any more has
/// its lookup removed; and the execution root over the membership names it
/// with every child of its stated kind.
#[test]
fn a_parks_membership_follows_its_ledger_by_admission() {
    use crate::kernel::{AdmittedEffect, EffectLedger, PendingEffect, Standing};
    use lash_core_store::checkpoint_graph::bodies::{ExecRoot, ExecStatus};
    use lash_core_store::checkpoint_graph::{CommitId, NodeKind};
    use lash_durable::domain::graph::ExecRootWrite;

    let projection = OneRecord("cell-args/v1");
    let projected = ArgumentProjection::project(&projection, &guest_args()).unwrap();
    let read = call("read", projected);
    let mut staging = staging();
    let mut book = AdmissionBook::default();
    let mut ledger = EffectLedger::new();
    let park = ledger.take_park();
    let operation = OperationId {
        run: park,
        ordinal: 1,
    };
    let request = FrozenRequest::tool(read.clone(), guest_args(), &projection).unwrap();
    let pinned = pin(&read, request.stage(&mut staging).unwrap());
    for frozen in [
        FrozenAdmission {
            effect: identity(0),
            wait: 0,
            effect_name: Some(EffectName::new("read").unwrap()),
            request,
            execution: Some((operation, pinned)),
        },
        FrozenAdmission {
            effect: identity(1),
            wait: 1,
            effect_name: None,
            request: FrozenRequest::Sleep {
                duration: std::time::Duration::from_millis(5),
                until_ms: 105,
            },
            execution: None,
        },
    ] {
        book.admit(&mut staging, &exec(), frozen).unwrap();
    }
    assert!(ledger.stand(
        identity(0),
        PendingEffect {
            wait: 0,
            effect: None,
            standing: Standing::Admitted(AdmittedEffect {
                operation,
                call: read.id.clone(),
            }),
        }
    ));
    assert!(ledger.stand(
        identity(1),
        PendingEffect {
            wait: 1,
            effect: None,
            standing: Standing::Sleeping { until_ms: 105 },
        }
    ));
    let membership = book.membership(&ledger).unwrap();
    assert_eq!(membership.entries.len(), 2);
    assert!(book.retire_unnamed(&exec(), &membership).is_empty());

    assert!(ledger.withdraw(lash_kernel_vm::WaitId(0)));
    ledger.consume(&identity(1));
    let membership = book.membership(&ledger).unwrap();
    assert_eq!(
        membership
            .entries
            .iter()
            .map(|entry| (entry.effect.clone(), entry.status))
            .collect::<Vec<_>>(),
        vec![(identity(0), MemberStatus::Released)]
    );
    let retired = book.retire_unnamed(&exec(), &membership);
    assert!(matches!(
        retired.as_slice(),
        [lash_durable::DomainWrite::EffectAdmission(EffectAdmissionWrite::Remove { effect, .. })]
            if *effect == effect_key(&identity(1))
    ));

    let state = staging
        .encode_value(NodeKind::KERNEL_WHOLE, &7_u64)
        .unwrap();
    let (root, write) = stage_exec_root(
        &mut staging,
        &exec(),
        Some(&membership),
        RootParts {
            state,
            status: ExecStatus::Parked,
            executable_identity: "doc".to_owned(),
            format: 1,
            host: None,
            envelope: None,
        },
        (None, None),
        CommitId([3; 32]),
    )
    .unwrap();
    assert!(
        matches!(write, lash_durable::DomainWrite::ExecRoot(ExecRootWrite::Put { root: put, .. }) if put == root)
    );
    let kept = collected(&staging, root);
    let mut decoder = GraphDecoder::new(&kept, DecodeLimits::within_total(1 << 24));
    let stored: ExecRoot = decoder.decode(root, NodeKind::EXEC_ROOT).unwrap();
    stored.check_children(root, &mut decoder).unwrap();
    let membership_node = stored.effect_membership.unwrap();
    let (loaded, _) =
        EffectMembership::load(&mut decoder, membership_node, &exec(), &projection).unwrap();
    assert_eq!(loaded, membership);
}
