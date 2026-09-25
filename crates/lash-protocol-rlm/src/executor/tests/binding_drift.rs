//! FIG-3587: a code cell's ambient binding set is journaled before its first
//! effect, and a redrive links against the recorded set.
//!
//! Each law runs a cell against a file-backed SQLite effect journal, changes
//! the registry — `app.a` removed, or its retry policy changed under the same
//! name — and redrives the same cell against a cold reopen of the journal. A
//! call the journal recorded a result for replays it; one that would reach
//! the drifted tool live refuses with the typed binding-drift code, which
//! parks the turn, and dispatches nothing — however often it is redriven. A
//! reworded descriptor is not drift: it only reaches the model's prompt, which
//! a redrive serves from the journal.

use super::replay_ordinals::{AppA, AppTools, Journal};
use super::*;

const DRIFTS: [AppA; 2] = [AppA::Removed, AppA::Retried];

fn drift_word(drift: AppA) -> &'static str {
    match drift {
        AppA::Removed => "missing",
        AppA::Retried => "changed",
        AppA::Registered | AppA::Described(_) => {
            unreachable!("{drift:?} is not a drift")
        }
    }
}

fn assert_binding_drift(run: &super::replay_ordinals::Run, drift: AppA) {
    assert_eq!(
        run.refusal_code(),
        Some(lash_core::RuntimeErrorCode::LashlangCellBindingDrift),
        "{:?}",
        run.nested
    );
    let message = &run.nested.as_ref().expect("the refusal").message;
    assert!(
        message.contains("`app.a`")
            && message.contains("tool:app_a")
            && message.contains(drift_word(drift)),
        "the refusal names the binding, its tool and how it drifted: {message}"
    );
    assert!(
        lash_core::RuntimeErrorCode::LashlangCellBindingDrift.parks_turn(),
        "binding drift parks the turn"
    );
}

/// The crash came after `app.a`'s recorded result: the redrive replays the
/// whole cell from its journal — the same answer, nothing dispatched, no new
/// journal row — although the live registry no longer offers `app.a` as the
/// cell recorded it.
#[test]
fn a_drifted_tool_with_a_recorded_result_replays_the_cell() {
    const CELL: &str = r#"
        const a = await app.a({ n: 1 });
        const b = await app.b({ n: 2 });
        finish({ a: a, b: b });
    "#;
    for drift in DRIFTS {
        block_on(async {
            let journal = Journal::open();
            let tools = AppTools::default();
            let first = journal.run(CELL, &tools).await;
            first.assert_clean();
            let recorded = journal.keys().await;
            assert_eq!(tools.dispatched(), 2);

            let redriven = journal.run(CELL, &tools.with_app_a(drift)).await;
            redriven.assert_clean();
            assert_eq!(
                redriven.response.terminal_finish, first.response.terminal_finish,
                "the cell answers byte-identically from its journal"
            );
            assert_eq!(tools.dispatched(), 2, "nothing is dispatched on redrive");
            assert_eq!(
                journal.keys().await,
                recorded,
                "the redrive journals nothing"
            );
        });
    }
}

/// The crash came before `app.a` was claimed: the call is needed live past
/// the frontier, so the redrive refuses with the binding-drift code before
/// anything is dispatched — and a second redrive refuses again, with nothing
/// dispatched, instead of looping on a hash conflict.
#[test]
fn a_drifted_tool_needed_live_refuses_with_nothing_dispatched() {
    const CELL: &str = r#"
        await app.b({ n: 1 });
        await app.a({ n: 2 });
        finish(1);
    "#;
    for drift in DRIFTS {
        block_on(async {
            let journal = Journal::open();
            let tools = AppTools::default();
            let crashing = journal.host().await;
            let faults = crashing.effect_journal_faults();
            faults.fail_next(
                lash_core::runtime::effect::effect_replay_driver::EffectJournalFaultPoint::Claim,
                &format!("{}:lk2:0000000001:attempt:1", "exec-code:ordinals"),
            );
            let crashed = journal.run_under(CELL, &tools, crashing).await;
            assert!(faults.fired(), "the injected crash fired at app.a's claim");
            assert!(crashed.nested.is_some(), "the crash aborts the cell");
            assert_eq!(tools.dispatched(), 1);

            let drifted = tools.with_app_a(drift);
            for _ in 0..2 {
                let redriven = journal.run(CELL, &drifted).await;
                assert_binding_drift(&redriven, drift);
                assert_eq!(tools.dispatched(), 1, "the drifted tool is never reached");
            }
        });
    }
}

/// The crash came after `app.a` was dispatched but before its result was
/// recorded: its attempt is on the journal but unsettled, so serving the call
/// would reach the tool live. The redrive refuses at the attempt, before its
/// claim.
#[test]
fn a_drifted_tool_whose_result_was_never_recorded_refuses() {
    const CELL: &str = r#"
        await app.b({ n: 1 });
        await app.a({ n: 2 });
        finish(1);
    "#;
    for drift in DRIFTS {
        block_on(async {
            let journal = Journal::open();
            let tools = AppTools::default();
            let crashing = journal.host().await;
            let faults = crashing.effect_journal_faults();
            faults.fail_next(
                lash_core::runtime::effect::effect_replay_driver::EffectJournalFaultPoint::Finalize,
                &format!("{}:lk2:0000000001:attempt:1", "exec-code:ordinals"),
            );
            let crashed = journal.run_under(CELL, &tools, crashing).await;
            assert!(
                faults.fired(),
                "the injected crash fired at app.a's finalize"
            );
            assert!(crashed.nested.is_some(), "the crash aborts the cell");
            let dispatched = tools.dispatched();

            let drifted = tools.with_app_a(drift);
            for _ in 0..2 {
                let redriven = journal.run(CELL, &drifted).await;
                assert_binding_drift(&redriven, drift);
                assert_eq!(tools.dispatched(), dispatched, "nothing is re-dispatched");
            }
        });
    }
}

/// A redrive under an unchanged registry reads the recorded binding set and
/// writes nothing new: the record is journaled once, on the first pass.
#[test]
fn an_unchanged_registry_replays_the_binding_set_once() {
    const CELL: &str = "const a = await app.a({ n: 1 }); finish(a);";
    block_on(async {
        let journal = Journal::open();
        let tools = AppTools::default();
        journal.run(CELL, &tools).await.assert_clean();
        let recorded = journal.keys().await;
        assert!(
            recorded
                .0
                .iter()
                .any(|key| key.ends_with("exec-code:ordinals:cell-tool-bindings")),
            "the binding set is journaled under the exec effect: {:?}",
            recorded.0
        );
        journal.run(CELL, &tools).await.assert_clean();
        assert_eq!(journal.keys().await, recorded);
        assert_eq!(tools.dispatched(), 1);
    });
}

/// A reworded descriptor never parks (FIG-3587): a call whose result the
/// journal does not hold runs the tool live, as any redrive past its frontier
/// does, and the cell completes.
#[test]
fn a_reworded_descriptor_never_parks() {
    const CELL: &str = r#"
        await app.b({ n: 1 });
        await app.a({ n: 2 });
        finish(1);
    "#;
    block_on(async {
        let journal = Journal::open();
        let tools = AppTools::default();
        let crashing = journal.host().await;
        let faults = crashing.effect_journal_faults();
        faults.fail_next(
            lash_core::runtime::effect::effect_replay_driver::EffectJournalFaultPoint::Claim,
            &format!("{}:lk2:0000000001:attempt:1", "exec-code:ordinals"),
        );
        let crashed = journal.run_under(CELL, &tools, crashing).await;
        assert!(faults.fired() && crashed.nested.is_some());
        journal
            .run(
                CELL,
                &tools.with_app_a(AppA::Described("reworded since the cell ran")),
            )
            .await
            .assert_clean();
        assert_eq!(
            tools.dispatched(),
            2,
            "app.a runs once, live, after the crash"
        );
    });
}

/// A deferred tool whose binding drifted, redriven while its completion wait
/// was never claimed: the attempt replays from the journal, and the wait —
/// which dispatches nothing — carries no served-only refusal and receives the
/// completion (FIG-3587).
#[test]
fn a_drifted_deferred_tool_still_waits_for_its_completion() {
    const CELL: &str = "const d = await app.d({ n: 1 }); finish(d);";
    block_on(async {
        let journal = Journal::open();
        let tools = AppTools::default().completing_on(&journal.path);
        let crashing = journal.host().await;
        let faults = crashing.effect_journal_faults();
        faults.fail_next(
            lash_core::runtime::effect::effect_replay_driver::EffectJournalFaultPoint::Claim,
            &format!("{}:lk2:0000000000:await", "exec-code:ordinals"),
        );
        let crashed = journal.run_under(CELL, &tools, crashing).await;
        assert!(faults.fired(), "the injected crash fired at app.d's await");
        assert!(crashed.nested.is_some(), "the crash aborts the cell");
        assert_eq!(tools.dispatched(), 1);

        let redriven = journal.run(CELL, &tools.with_app_d(AppA::Retried)).await;
        redriven.assert_clean();
        assert_eq!(
            tools.dispatched(),
            1,
            "the deferred tool is never re-dispatched"
        );
    });
}
