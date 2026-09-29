//! `negotiated_wire_both_directions` (ADR 0115 §6): the remote protocol and
//! the Restate wire, each crossed in both directions between N and N+1.
//!
//! **Remote protocol.** Each build's `remote-client` starts the other
//! build's `remote-host` as a child process and runs one turn through it.
//! N speaks `[V, V]` and N+1 `[V, V+1]`, so both directions select V: the
//! request, every stream item, the reply and the error all carry V, and N+1
//! drops its added field when it encodes V. N+1 against N+1 selects V+1 and
//! carries the field. A synthetic `[V+1, V+1]` peer against N is refused
//! `Unsupported`, typed on both sides, and its request is refused before
//! decode: the host starts no turn and the provider makes no call.
//!
//! **Restate.** With N+1 the newest deployment, an N caller's call runs on
//! N+1's handler and is answered at wire 1, which N reads, while an N+1
//! caller of the same handler selects 2. After a rollback, which registers
//! N at a fresh URI, an N+1 caller reaches N's handler and is answered at
//! wire 1. A call whose range is disjoint from N's is refused
//! `lash.wire_unsupported`, typed, and leaves the object without state.
//!
//! Every "runs on" is read from the invocation's pinned deployment, and
//! every "zero effects" and "exactly once" from the effects log the
//! serving nodes append to.

use anyhow::{Context, Result, ensure};
use lash_remote_protocol::{Negotiation, VersionRange};
use lash_upgrade_harness::harness::{CallSpec, Case, LASHCTL_N_ENV, Operator, block_on};
use lash_upgrade_harness::identity::BuildLabel;
use lash_upgrade_harness::node::objects::HandlerRefusal;
use lash_upgrade_harness::node::remote::{ClientReport, Unsupported};
use lash_upgrade_harness::node::served_by;

use crate::support::{GROUP, Leg, open_group_body, record, refused, replied};

/// Every message a client received was at `version`, and it received at
/// least one stream item, the reply and the error.
fn all_at(report: &ClientReport, version: u32) -> Result<()> {
    for frame in ["stream", "reply", "error"] {
        ensure!(
            report
                .received
                .iter()
                .any(|received| received.frame == frame),
            "{} received no {frame}: {:?}",
            report.build,
            report.received
        );
    }
    ensure!(
        report
            .received
            .iter()
            .all(|received| received.version == Some(version)),
        "{} expected every message at {version}: {:?}",
        report.build,
        report.received
    );
    Ok(())
}

/// One negotiated turn through the remote protocol: answered, at
/// `version`, by the host's own deployment, with exactly one model call.
fn negotiated_turn(case: &Case, report: &ClientReport, marker: &str, version: u32) -> Result<()> {
    ensure!(
        report.selected == Ok(version),
        "{} to {} selected {:?}, not {version}",
        report.build,
        report.host.build,
        report.selected
    );
    ensure!(report.request_version == version);
    all_at(report, version)?;
    ensure!(
        report.status.as_deref() == Some("Answered"),
        "the turn settled {:?}",
        report.status
    );
    let generation = report
        .host
        .generation
        .as_deref()
        .context("the host served no deployment")?;
    let expected = served_by(report.host.build, generation);
    ensure!(
        report.reply.as_deref() == Some(expected.as_str()),
        "expected the reply `{expected}`, got {:?}",
        report.reply
    );
    ensure!(
        report.host.turns_started == 1,
        "the host started {} turns",
        report.host.turns_started
    );
    ensure!(
        report.error.is_some(),
        "the invalid request was answered without an error"
    );
    let effects = case.effects_of(marker)?;
    ensure!(
        effects.len() == 1,
        "the turn made {} model calls, not exactly one: {effects:?}",
        effects.len()
    );
    Ok(())
}

fn remote_protocol(leg: &Leg, case: &Case) -> Result<()> {
    let (n, next) = (&leg.builds.n, &leg.builds.next);

    // N+1 to N: both speak V, and N+1's encoder of V drops its field.
    let marker = "remote-next-to-n";
    let next_to_n = next.remote_client(case, n, None, &case.session_id(marker), marker)?;
    record(leg, "remote-next-to-n.json", &next_to_n)?;
    let version = next_to_n.host.local.max();
    ensure!(
        next_to_n.host.local == VersionRange::exactly(version),
        "N speaks {}",
        next_to_n.host.local
    );
    ensure!(
        next_to_n.offered == VersionRange::between(version, version + 1),
        "N+1 offers {}",
        next_to_n.offered
    );
    ensure!(
        next_to_n.answer
            == Negotiation::Accept {
                supported: VersionRange::exactly(version),
                selected: version,
            },
        "N answered {:?}",
        next_to_n.answer
    );
    ensure!(next_to_n.host.build == BuildLabel::N);
    negotiated_turn(case, &next_to_n, marker, version)?;
    let request = &next_to_n.host.requests[0];
    ensure!(
        request.version == version && !request.carried_added_field,
        "N+1's request at {version} carried its added field: {request:?}"
    );

    // N to N+1: N+1 answers N's range with N's version.
    let marker = "remote-n-to-next";
    let n_to_next = n.remote_client(case, next, None, &case.session_id(marker), marker)?;
    record(leg, "remote-n-to-next.json", &n_to_next)?;
    ensure!(
        n_to_next.answer
            == Negotiation::Accept {
                supported: VersionRange::between(version, version + 1),
                selected: version,
            },
        "N+1 answered {:?}",
        n_to_next.answer
    );
    ensure!(n_to_next.host.build == BuildLabel::Next);
    negotiated_turn(case, &n_to_next, marker, version)?;
    let request = &n_to_next.host.requests[0];
    ensure!(
        !request.carried_added_field && request.note.is_none(),
        "{request:?}"
    );

    // N+1 to N+1 selects the newer version, and the field travels.
    let marker = "remote-next-to-next";
    let next_to_next = next.remote_client(case, next, None, &case.session_id(marker), marker)?;
    record(leg, "remote-next-to-next.json", &next_to_next)?;
    negotiated_turn(case, &next_to_next, marker, version + 1)?;
    let request = &next_to_next.host.requests[0];
    ensure!(
        request.carried_added_field && request.note.as_deref() == Some("from n+1"),
        "N+1's request at {} lost its field: {request:?}",
        version + 1
    );

    // A synthetic peer that speaks only V+1 is refused by N, typed on both
    // sides, and its request runs nothing.
    let marker = "remote-disjoint";
    let peer = VersionRange::exactly(version + 1);
    let disjoint = next.remote_client(case, n, Some(peer), &case.session_id(marker), marker)?;
    record(leg, "remote-disjoint.json", &disjoint)?;
    ensure!(
        disjoint.answer
            == Negotiation::Unsupported {
                local: VersionRange::exactly(version),
                peer,
            },
        "N answered the disjoint Hello {:?}",
        disjoint.answer
    );
    ensure!(
        disjoint.selected
            == Err(Unsupported {
                local: peer,
                peer: VersionRange::exactly(version),
            }),
        "the client selected {:?}",
        disjoint.selected
    );
    ensure!(
        disjoint.received.len() == 1 && disjoint.received[0].frame == "negotiation",
        "the disjoint request was answered with {:?}",
        disjoint.received
    );
    ensure!(
        disjoint.host.requests.len() == 1 && disjoint.host.requests[0].disposition == "refused",
        "{:?}",
        disjoint.host.requests
    );
    ensure!(
        disjoint.host.turns_started == 0,
        "the refused connection started {} turns",
        disjoint.host.turns_started
    );
    let effects = case.effects_of(marker)?;
    ensure!(
        effects.is_empty(),
        "the refused connection made model calls: {effects:?}"
    );
    Ok(())
}

fn restate_wire(
    leg: &Leg,
    case: &Case,
    n_first: &lash_upgrade_harness::harness::ServingNode,
) -> Result<()> {
    let (n, next) = (&leg.builds.n, &leg.builds.next);
    let view = case.view()?;

    // N+1 is the newest deployment: an N caller reaches its handler and is
    // answered at wire 1.
    let next_node = next.serve(case)?;
    let next_deployment = block_on(view.deployment_at(next_node.uri()?))?;
    let key = case.session_id("wire-n-caller");
    let opened = n.call(
        case,
        &CallSpec::object(GROUP, &key, "open").body(open_group_body(&view, &key)?),
    )?;
    record(leg, "restate-n-caller.json", &opened)?;
    let (wire, body) = replied(&opened)?;
    ensure!(
        opened.wire == VersionRange::exactly(1),
        "N states {}",
        opened.wire
    );
    ensure!(wire == 1, "N+1 answered N's call at wire {wire}");
    ensure!(body["type"] == "opened_fresh", "the open answered {body}");
    let invocations = block_on(view.invocations(GROUP, &key, "open"))?;
    ensure!(
        invocations.len() == 1
            && invocations[0].pinned_deployment_id.as_deref() == Some(next_deployment.id.as_str()),
        "N's call did not run on N+1's deployment {}: {invocations:?}",
        next_deployment.id
    );
    // The same handler answers N+1's own caller at the newer wire.
    let probed = next.call(case, &CallSpec::object(GROUP, &key, "probe"))?;
    let (wire, body) = replied(&probed)?;
    ensure!(
        probed.wire == VersionRange::between(1, 2) && wire == 2,
        "N+1 to N+1 answered at wire {wire} for {}",
        probed.wire
    );
    ensure!(body["type"] == "exists", "the probe answered {body}");

    // Rollback: N registers at a fresh URI, and N+1's deployment stays.
    let n_again = n.serve(case)?;
    ensure!(
        n_again.uri()? != n_first.uri()? && n_again.uri()? != next_node.uri()?,
        "the rollback reused a URI"
    );
    let n_deployment = block_on(view.deployment_at(n_again.uri()?))?;
    let deployments = block_on(view.deployments())?;
    ensure!(
        deployments
            .iter()
            .any(|deployment| deployment.id == next_deployment.id),
        "the rollback removed N+1's deployment: {deployments:?}"
    );

    // An N+1 caller reaches N's handler and is answered at wire 1.
    let key = case.session_id("wire-next-caller");
    let opened = next.call(
        case,
        &CallSpec::object(GROUP, &key, "open").body(open_group_body(&view, &key)?),
    )?;
    record(leg, "restate-next-caller.json", &opened)?;
    let (wire, body) = replied(&opened)?;
    ensure!(wire == 1, "N answered N+1's call at wire {wire}");
    ensure!(body["type"] == "opened_fresh", "the open answered {body}");
    let invocations = block_on(view.invocations(GROUP, &key, "open"))?;
    ensure!(
        invocations.len() == 1
            && invocations[0].pinned_deployment_id.as_deref() == Some(n_deployment.id.as_str()),
        "N+1's call did not run on N's deployment {}: {invocations:?}",
        n_deployment.id
    );

    // A caller whose range N cannot answer changes nothing.
    let key = case.session_id("wire-disjoint");
    let peer = VersionRange::exactly(2);
    let disjoint = next.call(
        case,
        &CallSpec::object(GROUP, &key, "open")
            .body(open_group_body(&view, &key)?)
            .wire(peer),
    )?;
    record(leg, "restate-disjoint.json", &disjoint)?;
    ensure!(
        *refused(&disjoint)?
            == HandlerRefusal::WireUnsupported {
                local: VersionRange::exactly(1),
                peer,
            },
        "N refused the disjoint call with {:?}",
        disjoint.outcome
    );
    let state = block_on(view.object_state(GROUP, &key))?;
    ensure!(state.is_empty(), "the disjoint call wrote state: {state:?}");
    let invocations = block_on(view.invocations(GROUP, &key, "open"))?;
    ensure!(
        invocations
            .iter()
            .all(|invocation| invocation.pinned_deployment_id.as_deref()
                == Some(n_deployment.id.as_str())),
        "the disjoint call ran off N's deployment: {invocations:?}"
    );

    n_again.stop()?;
    next_node.stop()?;
    Ok(())
}

/// Remote protocol: N+1 to N and N to N+1 select N's version, including
/// requests, replies, errors and streams. A synthetic peer of N+1's version
/// alone against N gets `Unsupported`, with zero effects. Restate: an N
/// caller reaches N+1's handlers and gets version-1 replies. After a
/// rollback an N+1 caller reaches N's handlers. A disjoint call changes
/// nothing.
#[test]
#[ignore = "needs both node builds, PostgreSQL and a restate-server: `just phase-a` runs it"]
fn negotiated_wire_both_directions() -> Result<()> {
    let leg = Leg::start("negotiated_wire_both_directions")?;
    let case = Case::postgres_database("wire", &leg.services, &leg.scratch)?;
    Operator::for_case(&case, LASHCTL_N_ENV)?.run("migrate", None)?;
    // The remote hosts serve deployments of their own, under a namespace
    // apart from the Restate legs'.
    remote_protocol(&leg, &case.beside("remote")?).context("the remote protocol")?;
    let n_node = leg.builds.n.serve(&case)?;
    restate_wire(&leg, &case, &n_node).context("the Restate wire")?;
    n_node.stop()?;
    Ok(())
}
