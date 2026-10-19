//! FIG-6810: a workflow whose body ends in an error no `catch` took ends
//! with a typed failure: the error's kind and the site that raised it as
//! its cause, the error's own message, never a dump of the value. A
//! library call has no activity state of its own: coverage counts it, and
//! the terminal names the statement it raised in.
//!
//! FIG-6817: a failed effect whose error a `catch` took reports itself
//! recovered; one nothing recovered from stays failed.

use super::*;
use lash::tools::{ToolCallOutcome, ToolFailure, ToolFailureCause};
use lash::workflow::document::{Site, SourceLanguage};
use lash::workflow::view::{self, CalleeView};

/// `pick(event)` reads a member of its event; `send(note)` hands
/// `tools.echo` a text where its schema takes an object; `triage(event)`
/// calls `tools.echo`, then reads a member of its event.
const TYPESCRIPT: &str = "export async function pick(event: any) {\n  const label = \"routine\";\n  if (event.urgent) {\n    return \"urgent\";\n  }\n  return label;\n}\n\nexport async function send(note: string) {\n  const sent = await tools.echo(note);\n  return sent;\n}\n\nexport async function triage(event: any) {\n  const echoed = await tools.echo({ value: \"seen\" });\n  if (event.urgent) {\n    return echoed;\n  }\n  return \"routine\";\n}\n";
const PYTHON: &str = "async def pick(event):\n    label = \"routine\"\n    if event[\"urgent\"]:\n        return \"urgent\"\n    return label\n\n\nasync def send(note: str):\n    sent = await tools.echo(note)\n    return sent\n\n\nasync def triage(event):\n    echoed = await tools.echo({\"value\": \"seen\"})\n    if event[\"urgent\"]:\n        return echoed\n    return \"routine\"\n";

/// One dialect's module, imported and published, entry by entry.
struct Module {
    language: SourceLanguage,
    source: &'static str,
    map: lash::workflow::document::SourceMap,
    pin: lash::process::HostArtifactPin,
    document: lash::workflow::document::DocumentBytes,
    environment: lash::process::ProcessEnv,
}

impl Module {
    async fn import(core: &lash::LashCore, language: SourceLanguage, source: &'static str) -> Self {
        let artifacts = core.host_artifacts();
        let environment = environment(false);
        let (document, source_map) = match artifacts
            .import_workflow(language, source, &environment, None)
            .await
            .expect("import answers")
        {
            lash::workflow::WorkflowImport::Imported {
                document,
                source_map,
                ..
            } => (document, source_map),
            other => panic!("the module imports: {other:?}\n{source}"),
        };
        Self {
            language,
            source,
            map: source_map,
            pin: lash::process::HostArtifactPin::mint(),
            document,
            environment,
        }
    }

    /// Starts `entry` with `args`, linking exactly `effects` and its
    /// statements counted, and answers its publication and the process.
    async fn start(
        &self,
        core: &lash::LashCore,
        entry: &str,
        effects: &[&str],
        args: serde_json::Value,
    ) -> (WorkflowPublication, lash_core::ProcessId) {
        let env_ref = core
            .host_artifacts()
            .publish_process_env(&self.pin, &linking(&self.environment, effects))
            .await
            .expect("the process environment publishes");
        let publication = published(
            core.host_artifacts()
                .publish_workflow(
                    &self.pin,
                    &self.document,
                    &Name::new(entry),
                    &self.environment,
                    None,
                )
                .await
                .expect("the entry publishes"),
        );
        let settings = core
            .processes()
            .kernel_engine_defaults()
            .expect("the kernel states its settings")
            .observing(lash::process::ObservationMode::Counts)
            .into_engine_settings();
        let serde_json::Value::Object(args) = args else {
            panic!("arguments are a record");
        };
        let process = core
            .processes()
            .start(
                lash::process::ProcessStartRequest::definition(
                    publication.definition.id.clone(),
                    Some(publication.definition.signature.clone()),
                    args,
                    lash::process::EngineLaunch::new(env_ref, Some(settings)),
                    lash::process::ProcessOriginator::host(),
                    lash::process::Lifetime::Detached,
                )
                .with_host_start_key(format!("{:?}-{entry}", self.language)),
                core.effect_host(),
            )
            .await
            .expect("the process starts")
            .process_id;
        (publication, process)
    }

    /// The source ranges the map gives `site`, as text.
    fn spelled(&self, site: &Site) -> Vec<&'static str> {
        self.map
            .sites
            .iter()
            .filter(|mapped| &mapped.site == site)
            .flat_map(|mapped| mapped.ranges.iter())
            .map(|range| &self.source[range.start..range.end])
            .collect()
    }
}

/// The failure a process ended with.
async fn failed(core: &lash::LashCore, process: &lash_core::ProcessId) -> ToolFailure {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        core.processes().await_output(process),
    )
    .await
    .expect("the process settles")
    .expect("the output reads");
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process settles with an output: {output:?}");
    };
    let ToolCallOutcome::Failure(failure) = output.outcome else {
        panic!("the process fails: {output:?}");
    };
    failure
}

/// The kind and site a failure's typed cause names.
fn uncaught(failure: &ToolFailure) -> (Option<String>, Site) {
    assert_eq!(failure.code, "process_runtime_error", "{failure:?}");
    match failure.cause.as_deref() {
        Some(ToolFailureCause::UncaughtError {
            error_kind,
            site: Some(site),
        }) => (error_kind.clone(), site.clone()),
        other => panic!("the failure's cause is the uncaught error: {other:?}"),
    }
}

/// A member read of `null` raises in the library call that reads it; a
/// refused tool argument raises at the `perform`, its error the tool's
/// failure. Each terminal names the error's kind and message and the site
/// that raised it, in both dialects.
#[tokio::test]
async fn an_uncaught_error_ends_a_workflow_with_its_kind_message_and_site() {
    let core = core().await;
    for (language, source, read, kind, message) in [
        (
            SourceLanguage::TypeScript,
            TYPESCRIPT,
            "event.urgent",
            "type_error",
            "Cannot read properties of undefined or null (reading 'urgent')",
        ),
        (
            SourceLanguage::Python,
            PYTHON,
            "event[\"urgent\"]",
            "TypeError",
            "'NoneType' object is not subscriptable",
        ),
    ] {
        let module = Module::import(&core, language, source).await;

        let (_, process) = module
            .start(&core, "pick", &[], serde_json::json!({"event": null}))
            .await;
        let failure = failed(&core, &process).await;
        let (error_kind, site) = uncaught(&failure);
        assert_eq!(
            (error_kind.as_deref(), failure.message.as_str()),
            (Some(kind), message),
            "{language:?}"
        );
        assert_eq!(site.unit, Unit::Function(Name::new("pick")), "{language:?}");
        assert!(
            module.spelled(&site).iter().any(|text| text.contains(read)),
            "{language:?}: {site} spells {:?}",
            module.spelled(&site)
        );

        let (publication, process) = module
            .start(
                &core,
                "send",
                &["tools.echo"],
                serde_json::json!({"note": "hello"}),
            )
            .await;
        let failure = failed(&core, &process).await;
        let (error_kind, site) = uncaught(&failure);
        assert_eq!(error_kind.as_deref(), Some("tool_failed"), "{language:?}");
        let tool = failure
            .raw
            .as_ref()
            .expect("the tool's failure is the error's data")
            .to_json_value();
        assert_eq!(tool["code"], "invalid_tool_args", "{language:?}: {tool}");
        assert_eq!(failure.message, tool["message"], "{language:?}");
        let document = publication_document(&core, &publication).await;
        let performs: Vec<_> = view::derive(&document)
            .expect("the entry's view")
            .execution_sites
            .into_iter()
            .filter(|action| {
                action.kind == view::SiteKind::Perform
                    && action.site.unit == Unit::Function(Name::new("send"))
            })
            .map(|action| action.site)
            .collect();
        assert_eq!(performs, std::slice::from_ref(&site), "{language:?}");
        assert!(
            module
                .spelled(&site)
                .iter()
                .any(|text| text.contains("tools.echo(note)")),
            "{language:?}: {site} spells {:?}",
            module.spelled(&site)
        );
    }
    core.shutdown().await.expect("shutdown");
}

/// A library call commits no state of its own, so the activity a host
/// attributes from the committed effect occurrences gives it none, while
/// the effect beside it has its occurrence's. The run's counts are the only
/// durable fact about a library call: an overlay row at one is live
/// evidence only, never settled, and `TerminalUnobserved` once the process
/// ends. A failed run's terminal names the statement its error was raised
/// in.
#[tokio::test]
async fn a_library_call_has_no_activity_state_and_its_coverage_counts_it() {
    use lash::workflow::ProcessWorkflowSiteState;
    let core = core().await;
    for (language, source) in [
        (SourceLanguage::TypeScript, TYPESCRIPT),
        (SourceLanguage::Python, PYTHON),
    ] {
        let module = Module::import(&core, language, source).await;
        let (publication, process) = module
            .start(
                &core,
                "triage",
                &["tools.echo"],
                serde_json::json!({"event": null}),
            )
            .await;
        let (_, raised) = uncaught(&failed(&core, &process).await);
        let document = publication_document(&core, &publication).await;
        let view = view::derive(&document).expect("the entry's view");
        let library: Vec<Site> = view
            .dataflow
            .actions
            .iter()
            .filter(|action| matches!(action.callee, Some(CalleeView::Library { .. })))
            .map(|action| action.site.clone())
            .collect();
        assert!(
            !library.is_empty(),
            "{language:?}: triage calls the library"
        );

        let snapshot = core
            .processes()
            .observe(&process)
            .snapshot()
            .await
            .expect("snapshot");
        let lash::process::ProcessReadView::Retained(read) = snapshot.read_view else {
            panic!("retained");
        };
        let observations: Vec<_> = read
            .effects
            .report
            .nodes()
            .flat_map(|node| node.occurrences.iter())
            .map(|occurrence| view::ActionObservation {
                identity: occurrence.at.clone(),
                state: match occurrence.outcome_class {
                    lash::process::ProcessEffectOutcomeClass::Success => {
                        view::ActionState::Completed
                    }
                    lash::process::ProcessEffectOutcomeClass::Failure => view::ActionState::Failed,
                    lash::process::ProcessEffectOutcomeClass::Cancelled => {
                        view::ActionState::Cancelled
                    }
                },
                outcome: None::<()>,
            })
            .collect();
        let activity = view
            .activity(view.document, &observations)
            .expect("the occurrences name this document's actions");
        let state = |kind: &dyn Fn(&Site) -> bool| {
            activity
                .iter()
                .filter(|node| kind(&node.site))
                .map(|node| node.state)
                .collect::<Vec<_>>()
        };
        let performs: Vec<Site> = view
            .execution_sites
            .iter()
            .filter(|action| {
                action.kind == view::SiteKind::Perform
                    && action.site.unit == Unit::Function(Name::new("triage"))
            })
            .map(|action| action.site.clone())
            .collect();
        assert_eq!(
            state(&|site| performs.contains(site)),
            [Some(view::ActionState::Completed)],
            "{language:?}"
        );
        assert!(
            state(&|site| library.contains(site))
                .iter()
                .all(Option::is_none),
            "{language:?}"
        );

        let overlay = read.workflow.expect("the process's workflow overlay");
        for row in &overlay.sites {
            if library
                .iter()
                .any(|site| site.position() == row.at.position())
            {
                assert_eq!(
                    (row.first_entered, row.last_settled, &row.state),
                    (None, None, &ProcessWorkflowSiteState::TerminalUnobserved),
                    "{language:?}: {row:?}"
                );
            }
        }
        let coverage = view.coverage(&overlay.ran_counts());
        assert!(
            coverage
                .iter()
                .any(|count| library.contains(&count.site) && count.runs > Some(0)),
            "{language:?}: {coverage:?}"
        );
        assert!(
            view.statements
                .iter()
                .any(|statement| statement.site == raised),
            "{language:?}: the terminal names a statement of the entry, {raised}"
        );
    }
    core.shutdown().await.expect("shutdown");
}

/// `guarded` catches `tools.echo`'s failure and returns; `nested` raises
/// it again unchanged from an inner catch (TypeScript) or an `except` that
/// does not match it (Python), and an outer catch takes it; `rethrown`
/// catches it and raises it again; `fallback` catches it and calls
/// `tools.echo` again from its catch, uncaught; `unguarded` catches
/// nothing. Each hands `tools.echo` a text where its schema takes an
/// object, so every call fails.
const RECOVERING_TYPESCRIPT: &str = "export async function nested(note: string) {\n  try {\n    try {\n      await tools.echo(note);\n    } catch (error) {\n      throw error;\n    }\n  } catch (error) {\n    return \"recovered\";\n  }\n  return \"sent\";\n}\n\nexport async function guarded(note: string) {\n  try {\n    await tools.echo(note);\n  } catch (error) {\n    return \"recovered\";\n  }\n  return \"sent\";\n}\n\nexport async function rethrown(note: string) {\n  try {\n    await tools.echo(note);\n  } catch (error) {\n    throw error;\n  }\n  return \"sent\";\n}\n\nexport async function fallback(note: string) {\n  try {\n    await tools.echo(note);\n  } catch (error) {\n    await tools.echo(note);\n  }\n  return \"sent\";\n}\n\nexport async function unguarded(note: string) {\n  const sent = await tools.echo(note);\n  return sent;\n}\n";
const RECOVERING_PYTHON: &str = "async def nested(note: str):\n    try:\n        try:\n            await tools.echo(note)\n        except KeyError:\n            return \"wrong\"\n    except Exception:\n        return \"recovered\"\n    return \"sent\"\n\n\nasync def guarded(note: str):\n    try:\n        await tools.echo(note)\n    except Exception:\n        return \"recovered\"\n    return \"sent\"\n\n\nasync def rethrown(note: str):\n    try:\n        await tools.echo(note)\n    except Exception:\n        raise\n    return \"sent\"\n\n\nasync def fallback(note: str):\n    try:\n        await tools.echo(note)\n    except Exception:\n        await tools.echo(note)\n    return \"sent\"\n\n\nasync def unguarded(note: str):\n    sent = await tools.echo(note)\n    return sent\n";

/// Runs `entry` of `module` to its end, `tools.echo` linked, and answers
/// whether it succeeded, with the overlay state of each `tools.echo` call
/// in `entry`, in document order, and the activity a host attributes from
/// those rows.
async fn echo_states(
    core: &lash::LashCore,
    module: &Module,
    entry: &str,
) -> (
    bool,
    Vec<lash::workflow::ProcessWorkflowSiteState>,
    Vec<Option<view::ActionState>>,
) {
    use lash::workflow::ProcessWorkflowSiteState;
    let (publication, process) = module
        .start(
            core,
            entry,
            &["tools.echo"],
            serde_json::json!({"note": "hello"}),
        )
        .await;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        core.processes().await_output(&process),
    )
    .await
    .expect("the process settles")
    .expect("the output reads");
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process settles with an output: {output:?}");
    };
    let succeeded = matches!(output.outcome, ToolCallOutcome::Success(_));
    let document = publication_document(core, &publication).await;
    let view = view::derive(&document).expect("the entry's view");
    let performs: Vec<Site> = view
        .execution_sites
        .iter()
        .filter(|action| {
            action.kind == view::SiteKind::Perform
                && action.site.unit == Unit::Function(Name::new(entry))
        })
        .map(|action| action.site.clone())
        .collect();
    let snapshot = core
        .processes()
        .observe(&process)
        .snapshot()
        .await
        .expect("snapshot");
    let lash::process::ProcessReadView::Retained(read) = snapshot.read_view else {
        panic!("retained");
    };
    let overlay = read.workflow.expect("the process's workflow overlay");
    let rows: Vec<_> = performs
        .iter()
        .map(|site| {
            overlay
                .sites
                .iter()
                .find(|row| site.position() == row.at.position())
                .unwrap_or_else(|| panic!("{entry}: no row at {site}: {overlay:?}"))
        })
        .collect();
    let observations: Vec<_> = rows
        .iter()
        .map(|row| view::ActionObservation {
            identity: row.at.clone(),
            state: match row.state {
                ProcessWorkflowSiteState::Failed => view::ActionState::Failed,
                ProcessWorkflowSiteState::Recovered => view::ActionState::Recovered,
                ref other => panic!("{entry}: every call fails: {other:?}"),
            },
            outcome: None::<()>,
        })
        .collect();
    let activity = view
        .activity(view.document, &observations)
        .expect("the rows name this document's actions");
    let drawn = performs
        .iter()
        .map(|site| {
            activity
                .iter()
                .find(|node| &node.site == site)
                .and_then(|node| node.state)
        })
        .collect();
    (
        succeeded,
        rows.into_iter().map(|row| row.state.clone()).collect(),
        drawn,
    )
}

/// A failed call whose error a `catch` took, so the run went on, reports
/// `recovered` in the process overlay and in the activity drawn from it,
/// also when an inner catch raised the same error again for an outer one
/// to take. Both dialects.
#[tokio::test]
async fn a_caught_failure_reports_recovered() {
    use lash::workflow::ProcessWorkflowSiteState;
    let core = core().await;
    for (language, source) in [
        (SourceLanguage::TypeScript, RECOVERING_TYPESCRIPT),
        (SourceLanguage::Python, RECOVERING_PYTHON),
    ] {
        let module = Module::import(&core, language, source).await;
        for entry in ["guarded", "nested"] {
            assert_eq!(
                echo_states(&core, &module, entry).await,
                (
                    true,
                    vec![ProcessWorkflowSiteState::Recovered],
                    vec![Some(view::ActionState::Recovered)],
                ),
                "{language:?} {entry}"
            );
        }
    }
    core.shutdown().await.expect("shutdown");
}

/// A failed call no `catch` took stays `failed`, in both dialects.
#[tokio::test]
async fn an_uncaught_failure_stays_failed() {
    use lash::workflow::ProcessWorkflowSiteState;
    let core = core().await;
    for (language, source) in [
        (SourceLanguage::TypeScript, RECOVERING_TYPESCRIPT),
        (SourceLanguage::Python, RECOVERING_PYTHON),
    ] {
        let module = Module::import(&core, language, source).await;
        assert_eq!(
            echo_states(&core, &module, "unguarded").await,
            (
                false,
                vec![ProcessWorkflowSiteState::Failed],
                vec![Some(view::ActionState::Failed)],
            ),
            "{language:?}"
        );
    }
    core.shutdown().await.expect("shutdown");
}

/// A `catch` that raises the error again has not recovered from it, and a
/// call that fails inside a `catch` with nothing around it to take its
/// error is not recovered by the `try` it sits in: both stay `failed`,
/// while the call that catch took is `recovered`. Both dialects.
#[tokio::test]
async fn a_failure_a_catch_raises_again_is_not_recovered() {
    use lash::workflow::ProcessWorkflowSiteState::{Failed, Recovered};
    let core = core().await;
    for (language, source) in [
        (SourceLanguage::TypeScript, RECOVERING_TYPESCRIPT),
        (SourceLanguage::Python, RECOVERING_PYTHON),
    ] {
        let module = Module::import(&core, language, source).await;
        assert_eq!(
            echo_states(&core, &module, "rethrown").await,
            (false, vec![Failed], vec![Some(view::ActionState::Failed)]),
            "{language:?}"
        );
        assert_eq!(
            echo_states(&core, &module, "fallback").await,
            (
                false,
                vec![Recovered, Failed],
                vec![
                    Some(view::ActionState::Recovered),
                    Some(view::ActionState::Failed)
                ],
            ),
            "{language:?}"
        );
    }
    core.shutdown().await.expect("shutdown");
}
