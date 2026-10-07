//! The composer's laws (FIG-5256): WRAP-REPLACE, OMIT and BOUNDS. SNAPSHOT
//! and RETENTION touch the store and run in `tests/store_backed` and
//! `lash_durable::laws`.

use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use super::*;

fn pool(workers: usize, queue: usize) -> PromptRenderPool {
    PromptRenderPool::new(
        NonZeroUsize::new(workers).unwrap(),
        NonZeroUsize::new(queue).unwrap(),
    )
}

async fn compose_over(
    catalog: &PromptCatalog,
    plan: &PromptPlan,
    namespaces: BTreeMap<String, CommittedPluginNamespace>,
    pool: &PromptRenderPool,
) -> Result<ComposedPrompt, PromptCompositionError> {
    catalog
        .compose(plan, &PromptPurpose::Turn, Arc::new(cut(namespaces)), pool)
        .await
}

fn recorded(text: &str) -> RecordedSectionText {
    SectionText::text(text).record()
}

fn limits(section: u32, total: u32, budget_ms: u32) -> PromptPlan {
    PromptPlan {
        limits: PromptLimits {
            max_section_bytes: NonZeroU32::new(section).unwrap(),
            max_total_bytes: NonZeroU32::new(total).unwrap(),
            render_budget_ms: NonZeroU32::new(budget_ms).unwrap(),
            ..PromptLimits::DEFAULT
        },
        ..PromptPlan::default()
    }
}

/// WRAP-REPLACE: a trusted wrapper replaces a protocol section's text
/// outright, and a later wrapper in the chain sees that replacement, not the
/// protocol's text. The snapshot keeps all three: the base, each wrapper's
/// output and the final text.
#[tokio::test]
async fn a_wrapper_replaces_protocol_text_and_a_later_wrapper_sees_the_replacement() {
    let catalog = catalog(vec![
        (
            "lash.protocol",
            section(
                "intro",
                PromptPlacement::InitialInstructions,
                "protocol intro",
            ),
        ),
        (
            "memory",
            Box::new(|reg| {
                reg.prompt().wrap(
                    PromptWrapSpec::new(wrap_key("replace"), id("lash.protocol", "intro")),
                    Arc::new(
                        |_: &PromptInput<'_>,
                         _: PromptWrapTarget<'_>,
                         previous: SectionText|
                         -> Result<SectionText, PromptRenderError> {
                            assert_eq!(previous, SectionText::text("protocol intro"));
                            Ok(SectionText::text("memory intro"))
                        },
                    ),
                )
            }),
        ),
        (
            "audit",
            Box::new(|reg| {
                reg.prompt().wrap(
                    PromptWrapSpec::new(wrap_key("enclose"), id("lash.protocol", "intro")),
                    enclose("audit"),
                )
            }),
        ),
    ])
    .unwrap();

    let composed = compose_over(
        &catalog,
        &PromptPlan::default(),
        BTreeMap::new(),
        &pool(2, 8),
    )
    .await
    .unwrap();

    assert_eq!(
        composed.initial_instructions.as_deref(),
        Some("audit(memory intro)")
    );
    assert_eq!(composed.current_context, None);
    let [section] = composed.snapshot.sections.as_slice() else {
        panic!("one section: {:?}", composed.snapshot.sections);
    };
    assert_eq!(section.base, recorded("protocol intro"));
    assert_eq!(
        section
            .wraps
            .iter()
            .map(|applied| (applied.wrap.owner.as_str(), applied.output.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("memory", recorded("memory intro")),
            ("audit", recorded("audit(memory intro)")),
        ]
    );
    assert_eq!(section.value, recorded("audit(memory intro)"));
    assert_eq!(
        composed.texts.values().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "protocol intro".to_string(),
            "memory intro".to_string(),
            "audit(memory intro)".to_string(),
        ])
    );
}

fn memory(note: Option<&str>) -> BTreeMap<String, CommittedPluginNamespace> {
    let values = note
        .map(|note| BTreeMap::from([("note".to_string(), serde_json::json!(note))]))
        .unwrap_or_default();
    BTreeMap::from([(
        "memory".to_string(),
        CommittedPluginNamespace::new(1, values),
    )])
}

/// OMIT: a section that omits in this call carries no text, and nothing of
/// what an earlier call showed stands in. Empty text is an omission, and a
/// placement whose sections all omit carries nothing.
#[tokio::test]
async fn an_omitted_section_leaves_no_earlier_text() {
    let catalog = catalog(vec![
        (
            "host",
            section("rules", PromptPlacement::InitialInstructions, "rules"),
        ),
        (
            "memory",
            Box::new(|reg| {
                reg.prompt().section(
                    PromptSectionSpec::new(key("note"), PromptPlacement::CurrentContext),
                    Arc::new(
                        |input: &PromptInput<'_>| -> Result<SectionText, PromptRenderError> {
                            Ok(match input.state().get_as::<String>("note")? {
                                Some(note) => SectionText::Text(note),
                                None => SectionText::Omit,
                            })
                        },
                    ),
                )
            }),
        ),
    ])
    .unwrap();
    let plan = PromptPlan::default();
    let pool = pool(2, 8);

    let shown = compose_over(&catalog, &plan, memory(Some("remember X")), &pool)
        .await
        .unwrap();
    assert_eq!(shown.current_context.as_deref(), Some("remember X"));

    for namespaces in [memory(None), memory(Some(""))] {
        let omitted = compose_over(&catalog, &plan, namespaces, &pool)
            .await
            .unwrap();
        assert_eq!(omitted.initial_instructions.as_deref(), Some("rules"));
        assert_eq!(omitted.current_context, None, "no earlier text stands in");
        let note = &omitted.snapshot.sections[1];
        assert_eq!(note.section, id("memory", "note"));
        assert_eq!(note.base, RecordedSectionText::Omitted);
        assert_eq!(note.value, RecordedSectionText::Omitted);
        assert!(
            !omitted.texts.values().any(|text| text.contains("remember")),
            "the snapshot keeps no earlier text: {:?}",
            omitted.texts
        );
    }
}

/// BOUNDS: a section or wrapper output over the per-section limit, a total
/// over the call's limit, a refusal and a panic each compose nothing, and
/// each failure names its site. Oversize is refused, never truncated.
#[tokio::test]
async fn an_oversized_refused_or_panicking_render_composes_nothing_and_names_its_site() {
    let pool = pool(2, 16);
    let plan = limits(8, 10, 2_000);

    let oversized = catalog(vec![(
        "big",
        section("s", PromptPlacement::InitialInstructions, "123456789"),
    )])
    .unwrap();
    match compose_over(&oversized, &plan, BTreeMap::new(), &pool).await {
        Err(PromptCompositionError::SectionTooLarge { site, bytes, limit }) => {
            assert_eq!((bytes, limit), (9, 8));
            assert!(
                matches!(*site, PromptRenderSite::Base { section, .. } if section == id("big", "s"))
            );
        }
        other => panic!("an oversized base composed {other:?}"),
    }

    let grown = catalog(vec![
        (
            "base",
            section("s", PromptPlacement::InitialInstructions, "1234"),
        ),
        (
            "grow",
            Box::new(|reg| {
                reg.prompt().wrap(
                    PromptWrapSpec::new(wrap_key("w"), id("base", "s")),
                    enclose("grow"),
                )
            }),
        ),
    ])
    .unwrap();
    match compose_over(&grown, &plan, BTreeMap::new(), &pool).await {
        Err(PromptCompositionError::SectionTooLarge { site, .. }) => {
            assert!(
                matches!(*site, PromptRenderSite::Wrap { wrap, ordinal: 0, .. } if wrap.owner == "grow")
            );
        }
        other => panic!("an oversized wrapper output composed {other:?}"),
    }

    let total = catalog(vec![
        (
            "a",
            section("s", PromptPlacement::InitialInstructions, "aaaaaa"),
        ),
        ("b", section("s", PromptPlacement::CurrentContext, "bbbbbb")),
    ])
    .unwrap();
    assert_eq!(
        compose_over(&total, &plan, BTreeMap::new(), &pool).await,
        Err(PromptCompositionError::TotalTooLarge {
            bytes: 12,
            limit: 10
        })
    );

    let refused = catalog(vec![
        (
            "ok",
            section("s", PromptPlacement::InitialInstructions, "ok"),
        ),
        (
            "refuses",
            Box::new(|reg| {
                reg.prompt().section(
                    PromptSectionSpec::new(key("s"), PromptPlacement::InitialInstructions),
                    Arc::new(|_: &PromptInput<'_>| Err(PromptRenderError::new("no"))),
                )
            }),
        ),
    ])
    .unwrap();
    match compose_over(&refused, &plan, BTreeMap::new(), &pool).await {
        Err(PromptCompositionError::Render { site, error }) => {
            assert_eq!(error, PromptRenderError::new("no"));
            assert!(
                matches!(*site, PromptRenderSite::Base { section, .. } if section == id("refuses", "s"))
            );
        }
        other => panic!("a refusal composed {other:?}"),
    }

    let panicking = catalog(vec![
        (
            "base",
            section("s", PromptPlacement::CurrentContext, "base"),
        ),
        (
            "panics",
            Box::new(|reg| {
                reg.prompt().wrap(
                    PromptWrapSpec::new(wrap_key("w"), id("base", "s")),
                    Arc::new(
                        |_: &PromptInput<'_>,
                         _: PromptWrapTarget<'_>,
                         _: SectionText|
                         -> Result<SectionText, PromptRenderError> {
                            panic!("a wrapper panics")
                        },
                    ),
                )
            }),
        ),
    ])
    .unwrap();
    match compose_over(&panicking, &plan, BTreeMap::new(), &pool).await {
        Err(PromptCompositionError::Panicked { site }) => {
            assert!(
                matches!(*site, PromptRenderSite::Wrap { wrap, target, .. } if wrap.owner == "panics" && target == id("base", "s"))
            );
        }
        other => panic!("a panic composed {other:?}"),
    }
}

/// A section whose renderer reports when it starts, then holds its worker
/// until `release` sends or closes, and records that it finished.
fn holding() -> (
    Register,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
    Arc<AtomicBool>,
) {
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (started, starts) = std::sync::mpsc::channel::<()>();
    let released = Arc::new(std::sync::Mutex::new(released));
    let started = Arc::new(std::sync::Mutex::new(started));
    let finished = Arc::new(AtomicBool::new(false));
    let renderer = {
        let finished = Arc::clone(&finished);
        Arc::new(
            move |_: &PromptInput<'_>| -> Result<SectionText, PromptRenderError> {
                let _ = started.lock_recover().send(());
                let _ = released
                    .lock_recover()
                    .recv_timeout(Duration::from_secs(10));
                finished.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(SectionText::text("late"))
            },
        )
    };
    let register: Register = Box::new(move |reg| {
        reg.prompt().section(
            PromptSectionSpec::new(key("s"), PromptPlacement::InitialInstructions),
            renderer.clone(),
        )
    });
    (register, release, starts, finished)
}

/// BOUNDS: a render that outlasts the budget composes nothing, at the
/// budget rather than when the render ends. Its late result is dropped, the
/// renders queued behind it never start, and the next call composes
/// normally.
#[tokio::test]
async fn a_render_past_the_budget_composes_nothing_and_its_late_result_is_ignored() {
    let (slow, release, starts, finished) = holding();
    let later_runs = Arc::new(AtomicUsize::new(0));
    let later = {
        let later_runs = Arc::clone(&later_runs);
        Arc::new(
            move |_: &PromptInput<'_>| -> Result<SectionText, PromptRenderError> {
                later_runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(SectionText::text("later"))
            },
        )
    };
    let catalog = catalog(vec![
        ("slow", slow),
        (
            "later",
            Box::new(move |reg| {
                reg.prompt().section(
                    PromptSectionSpec::new(key("s"), PromptPlacement::InitialInstructions),
                    later.clone(),
                )
            }),
        ),
    ])
    .unwrap();
    // One worker: the slow render holds it, and the later render waits in
    // the queue behind it.
    let pool = pool(1, 4);

    assert_eq!(
        compose_over(&catalog, &limits(1024, 4096, 200), BTreeMap::new(), &pool).await,
        Err(PromptCompositionError::BudgetExceeded { budget_ms: 200 })
    );
    starts.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        !finished.load(std::sync::atomic::Ordering::SeqCst),
        "the call failed at its budget, not when the render ended"
    );

    release.send(()).unwrap();
    let next = compose_over(
        &catalog_with_fast_section(),
        &PromptPlan::default(),
        BTreeMap::new(),
        &pool,
    )
    .await
    .unwrap();
    assert_eq!(next.initial_instructions.as_deref(), Some("fast"));
    assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        later_runs.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "renders queued behind the budget never ran"
    );
}

/// BOUNDS: a pool whose queue is full refuses a call's renders instead of
/// queueing them without bound, and the refused call composes nothing.
#[tokio::test]
async fn a_full_render_queue_refuses_instead_of_growing() {
    let (holder, release, starts, _) = holding();
    let holder = catalog(vec![("holder", holder)]).unwrap();
    let pool = pool(1, 1);
    assert_eq!(
        compose_over(&holder, &limits(1024, 4096, 200), BTreeMap::new(), &pool).await,
        Err(PromptCompositionError::BudgetExceeded { budget_ms: 200 })
    );
    starts.recv_timeout(Duration::from_secs(10)).unwrap();

    let two = catalog(vec![
        ("a", section("s", PromptPlacement::InitialInstructions, "a")),
        ("b", section("s", PromptPlacement::InitialInstructions, "b")),
    ])
    .unwrap();
    assert_eq!(
        compose_over(&two, &PromptPlan::default(), BTreeMap::new(), &pool).await,
        Err(PromptCompositionError::RenderersBusy { capacity: 1 })
    );
    drop(release);
}

fn catalog_with_fast_section() -> PromptCatalog {
    catalog(vec![(
        "fast",
        section("s", PromptPlacement::InitialInstructions, "fast"),
    )])
    .unwrap()
}
