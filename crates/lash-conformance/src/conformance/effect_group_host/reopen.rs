use pretty_assertions::assert_eq;

use super::*;

/// A reopen is fenced on the group's shape, and reopening a group this host is
/// already running dispatches nothing.
///
/// Both halves matter for the same reason: a shrunk child vec under one key
/// renumbers every rank above the truncation, and a second dispatch doubles
/// every side effect the first is still producing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn group_reopen_retention_and_rank_exhaustion_contract<F: Fn() -> Host>(
    make: &F,
    prefix: &str,
) {
    for wake in [
        GroupWakePolicy::All,
        GroupWakePolicy::First,
        GroupWakePolicy::FirstSuccess,
    ] {
        for disposition in [RUN, LoserPolicy::Cancel] {
            let case = format!("{prefix}-{wake:?}-{disposition:?}");
            let host = make();
            let scoped = host
                .scoped(admit(scope(&case, "reopen")))
                .expect("a scope binds");
            let key = group_key(&case, "reopen");
            let runs = Arc::new(AtomicUsize::new(0));
            let counted = |runs: &Arc<AtomicUsize>, position: usize| {
                let runs = Arc::clone(runs);
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    Ok(outcome_of(position))
                })
            };

            for children in [
                Vec::new(),
                vec![child(scoped.execution_scope(), &key, 0).in_effect_group(
                    "foreign",
                    0,
                    wake,
                    disposition,
                )],
                vec![child(scoped.execution_scope(), &key, 0).in_effect_group(
                    &key,
                    1,
                    wake,
                    disposition,
                )],
                vec![child(scoped.execution_scope(), &key, 0).in_effect_group(
                    &key,
                    0,
                    if wake == GroupWakePolicy::All {
                        GroupWakePolicy::First
                    } else {
                        GroupWakePolicy::All
                    },
                    disposition,
                )],
                vec![child(scoped.execution_scope(), &key, 0).in_effect_group(
                    &key,
                    0,
                    wake,
                    if disposition == RUN {
                        LoserPolicy::Cancel
                    } else {
                        RUN
                    },
                )],
            ] {
                let refused = RuntimeEffectGroup::try_new(
                    RuntimeEffectInvocation::new(
                        EffectAddress::new(
                            scoped.execution_scope().clone(),
                            format!("{key}:invalid"),
                        )
                        .expect("valid address"),
                        RuntimeAttribution::none(),
                        "group",
                    ),
                    &key,
                    children,
                    wake,
                    disposition,
                )
                .expect_err("empty and inconsistent memberships are rejected before the host");
                assert_eq!(
                    refused.code,
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape
                );
            }
            let mut handle = open(
                &scoped,
                &key,
                2,
                wake,
                disposition,
                vec![counted(&runs, 0), counted(&runs, 1)],
            )
            .await;
            next(&scoped, &mut handle).await.expect("rank 1 is served");
            next(&scoped, &mut handle).await.expect("rank 2 is served");
            assert_eq!(runs.load(Ordering::SeqCst), 2, "each child ran once");

            // A reopen of the same shape is legal and must not run anything again.
            let reopened = scoped
                .controller()
                .open_effect_group(staged(
                    group(scoped.execution_scope(), &key, 2, wake, disposition),
                    vec![counted(&runs, 0), counted(&runs, 1)],
                ))
                .await
                .expect("a reopen of the same shape is legal");
            assert_eq!(reopened.consumed(), 0, "a reopened handle starts at zero");
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(
                runs.load(Ordering::SeqCst),
                2,
                "a reopen must not run a child a second time"
            );

            for (width, changed_wake, changed_disposition) in [
                (1, wake, disposition),
                (3, wake, disposition),
                (2, GroupWakePolicy::All, disposition),
                (2, GroupWakePolicy::First, disposition),
                (2, GroupWakePolicy::FirstSuccess, disposition),
                (2, wake, RUN),
                (2, wake, LoserPolicy::Cancel),
            ] {
                if width == 2 && changed_wake == wake && changed_disposition == disposition {
                    continue;
                }
                let error = scoped
                    .controller()
                    .open_effect_group(staged(
                        group(
                            scoped.execution_scope(),
                            &key,
                            width,
                            changed_wake,
                            changed_disposition,
                        ),
                        (0..width)
                            .map(|position| counted(&runs, position))
                            .collect(),
                    ))
                    .await
                    .expect_err("a changed recorded shape is refused before dispatch");
                assert_eq!(error.code, crate::RuntimeErrorCode::RuntimeEffectGroupShape);
            }
            let reader = make();
            let resumed = reader
                .scoped(admit(scope(&case, "reopen")))
                .expect("a fresh host binds");
            let mut restored =
                EffectGroupHandle::restored(&key, 2, 0).expect("restore the saved cursor");
            let mut positions = Vec::new();
            for _ in 0..2 {
                let rank = next(&resumed, &mut restored)
                    .await
                    .expect("retained payload remains readable");
                assert!(
                    matches!(rank.outcome, Ok(RuntimeEffectOutcome::LanguageRuntimeValue { ref value })
            if value == &serde_json::json!({ "position": rank.position }))
                );
                positions.push(rank.position);
            }
            positions.sort_unstable();
            assert_eq!(positions, vec![0, 1]);
            let before = restored.consumed();
            let exhausted = next(&resumed, &mut restored)
                .await
                .expect_err("exhaustion refuses another rank");
            assert_eq!(
                exhausted.code,
                crate::RuntimeErrorCode::RuntimeEffectGroupShape
            );
            assert_eq!(restored.consumed(), before);
            assert_eq!(
                runs.load(Ordering::SeqCst),
                2,
                "refusals and retention reads execute no body"
            );
            close(&scoped, reopened, disposition)
                .await
                .expect("the group closes");
            close(&scoped, handle, disposition)
                .await
                .expect("closing an already-closed group is idempotent");
        }
    }
}
