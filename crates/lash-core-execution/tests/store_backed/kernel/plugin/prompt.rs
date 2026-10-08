//! SNAPSHOT (FIG-5256): an admitted call's prompt snapshot reads back from
//! the store with every base, wrapper and final text byte for byte, and no
//! renderer or wrapper runs to produce it. Unchanged text is shared across
//! calls.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_durable::domain::{ModelCallId, PromptCallKey};
use lash_durable::{ActorKey, CommitLabel, FormatSet, MailTx, NodeId, NodeSpec};

use crate::plugin::prompt::{
    OfferedTools, ProjectedHistoryStats, PromptCall, PromptCatalog, PromptInput, PromptModel,
    PromptPlacement, PromptPlan, PromptPurpose, PromptRenderError, PromptSectionId,
    PromptSectionKey, PromptSectionSpec, PromptTextRef, PromptWrapKey, PromptWrapSpec,
    PromptWrapTarget, SectionText, admission_record, load_admitted_call,
};
use crate::testing::prompt::{ComposedPrompt, PromptCut, PromptCutParts, compose, namespace};
use crate::{PromptRegistration, SessionId};

const SESSION: &str = "prompt-snapshot";

fn key(local: &str) -> PromptSectionKey {
    PromptSectionKey::new(local).expect("valid section key")
}

fn cut(call: u32, note: &str) -> PromptCut {
    crate::testing::prompt::cut(PromptCutParts {
        call: PromptCall {
            session_id: SessionId::from(SESSION),
            frame: None,
            run: None,
            turn: None,
            iteration: 0,
            call,
            purpose: PromptPurpose::Turn,
        },
        config: crate::AdmittedPluginConfig::default(),
        session: None,
        offered: OfferedTools::default(),
        model: PromptModel::default(),
        history: ProjectedHistoryStats::default(),
        namespaces: BTreeMap::from([(
            "memory".to_string(),
            namespace(
                u64::from(call),
                BTreeMap::from([("note".to_string(), serde_json::json!(note))]),
            ),
        )]),
    })
}

/// A protocol intro, a memory note and a memory wrapper over the intro,
/// each counting its runs in `runs`.
fn catalog(runs: &Arc<AtomicUsize>) -> PromptCatalog {
    let intro = {
        let runs = Arc::clone(runs);
        Arc::new(
            move |_: &PromptInput<'_>| -> Result<SectionText, PromptRenderError> {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(SectionText::text("protocol intro"))
            },
        )
    };
    let note = {
        let runs = Arc::clone(runs);
        Arc::new(
            move |input: &PromptInput<'_>| -> Result<SectionText, PromptRenderError> {
                runs.fetch_add(1, Ordering::SeqCst);
                let note = input.state().get_as::<String>("note")?.unwrap_or_default();
                Ok(SectionText::Text(format!("note: {note}")))
            },
        )
    };
    let wrap = {
        let runs = Arc::clone(runs);
        Arc::new(
            move |_: &PromptInput<'_>,
                  _: PromptWrapTarget<'_>,
                  previous: SectionText|
                  -> Result<SectionText, PromptRenderError> {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(SectionText::Text(format!(
                    "{} (with memory)",
                    previous.as_text().unwrap_or_default()
                )))
            },
        )
    };
    let plugins: Vec<(&'static str, PromptRegistration)> = vec![
        (
            "lash.protocol",
            Box::new(move |reg| {
                reg.prompt().section(
                    PromptSectionSpec::new(key("intro"), PromptPlacement::InitialInstructions),
                    intro.clone(),
                )
            }),
        ),
        (
            "memory",
            Box::new(move |reg| {
                reg.prompt().section(
                    PromptSectionSpec::new(key("note"), PromptPlacement::CurrentContext),
                    note.clone(),
                )?;
                reg.prompt().wrap(
                    PromptWrapSpec::new(
                        PromptWrapKey::new("intro").expect("valid wrap key"),
                        PromptSectionId::new("lash.protocol", key("intro")),
                    ),
                    wrap.clone(),
                )
            }),
        ),
    ];
    crate::prompt_catalog(plugins).expect("the catalog registers")
}

fn call_key(call: u32) -> PromptCallKey {
    PromptCallKey {
        session: SessionId::from(SESSION),
        call: ModelCallId::Turn {
            run: lash_sansio::TurnId::from("prompt-turn"),
            ordinal: call,
        },
    }
}

/// The request template call `call` admits with its prompt: the first a
/// lone literal, the second an image slot between two literals.
/// The response context call `call` is admitted with: its own scope, as a
/// send names its attempt, over the model, the output it asks for and the
/// one tool it offers.
fn response(call: u32) -> lash_sansio::llm::types::ResponseContext {
    use lash_sansio::llm::types::{
        LlmOutputSpec, LlmRequestScope, ResponseContext, ResponseContract, ToolCallContract,
    };
    ResponseContext::recorded(
        LlmRequestScope {
            attempt: Some(call),
            ..LlmRequestScope::new(SESSION, "frame", format!("{SESSION}:call:{call}"))
        },
        Arc::new(ResponseContract {
            model: crate::testing::test_llm_profile_config(
                "model",
                crate::testing::test_llm_profile_metadata("model"),
            ),
            output_spec: Some(LlmOutputSpec::JsonObject),
            tools: vec![ToolCallContract {
                name: "lookup".to_owned(),
                input_schema: crate::SchemaContract::admit(serde_json::json!({
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                }))
                .expect("a valid tool input schema"),
            }],
        }),
    )
}

fn template(call: u32) -> lash_sansio::llm::types::RecordedRequestTemplate {
    use lash_sansio::llm::attachment_delivery::{AttachmentPosition, ProviderAccepts};
    use lash_sansio::llm::types::{AttachmentSlot, RecordedRequestTemplate, SlotCodec};
    let route = lash_sansio::llm::types::ProviderRouteIdentity {
        provider: "test".into(),
        endpoint: "https://provider.test/v1".into(),
        model: "model".into(),
    };
    if call == 1 {
        return RecordedRequestTemplate::literal(route, true, None, format!("{{\"call\":{call}}}"))
            .expect("the literal is JSON");
    }
    let mut builder = RecordedRequestTemplate::builder(route, true, None);
    builder
        .literal(format!("{{\"call\":{call},\"image\":"))
        .attachment(AttachmentSlot {
            reference: lash_sansio::AttachmentRef {
                id: lash_sansio::AttachmentId::parse("ab".repeat(32)).expect("a digest id"),
                media_type: lash_sansio::MediaType::parse("image/png").expect("a media type"),
                byte_len: 4,
                type_metadata: None,
                label: Some("shot".to_owned()),
            },
            position: AttachmentPosition::Message,
            accepts: ProviderAccepts {
                bytes: true,
                url: true,
                provider_file: None,
            },
            codec: SlotCodec {
                name: "lash.canonical".into(),
                revision: 1,
            },
        })
        .literal("}");
    builder.finish().expect("a valid template")
}

#[tokio::test]
async fn an_admitted_calls_snapshot_reads_back_byte_for_byte_without_any_renderer() {
    let runs = Arc::new(AtomicUsize::new(0));
    let catalog = catalog(&runs);
    let mut composed = Vec::<ComposedPrompt>::new();
    for (call, note) in [(1, "first"), (2, "second")] {
        composed.push(
            compose(
                &catalog,
                &PromptPlan::default(),
                &PromptPurpose::Turn,
                cut(call, note),
            )
            .await
            .expect("the call composes"),
        );
    }
    assert_eq!(runs.load(Ordering::SeqCst), 6, "three renders per call");

    let stores = crate::support::sqlite_memory_store_set().await;
    let durable = stores.durable_store();
    let formats = FormatSet::new("prompt-snapshot-formats");
    let actor = ActorKey::session(SESSION).expect("a session actor key");
    let mut create = MailTx::new();
    create.create_actor(actor.clone(), formats.clone());
    lash_durable::DurableStore::commit_mail(&durable, create, CommitLabel::new("law.create"))
        .await
        .expect("create the session actor");
    let node = lash_durable::DurableStore::register_node(
        &durable,
        &NodeSpec {
            node: NodeId::new("prompt-owner"),
            decodes: vec![formats],
            ttl_millis: 15_000,
        },
    )
    .await
    .expect("register the owner");
    let claimed = lash_durable::DurableStore::claim(&durable, &node, 1)
        .await
        .expect("claim the session");
    let mut tx = lash_durable::DurableStore::begin(&durable, &actor, claimed[0].epoch)
        .await
        .expect("begin the admission");
    for (call, prompt) in (1..).zip(&composed) {
        tx.write(
            admission_record(
                call_key(call),
                Some(prompt),
                &template(call),
                &response(call),
                None,
            )
            .expect("the admission encodes"),
        );
    }
    lash_durable::DurableStore::commit(&durable, tx, CommitLabel::new("model.start"))
        .await
        .expect("admit both calls");

    // A second runtime over the same store reads the snapshots back.
    let reader = stores.reopen().await.expect("reopen").durable_store();
    for (call, prompt) in (1..).zip(&composed) {
        let admitted = load_admitted_call(&reader, &call_key(call))
            .await
            .expect("the admission loads")
            .expect("the call is admitted");
        assert_eq!(
            *admitted.template,
            template(call),
            "the template, its literals and slots, reads back byte for byte"
        );
        // The response context reads back as admitted: the call's scope
        // with no attempt (each send names its own), and its contract.
        let recorded = response(call);
        assert_eq!(
            admitted.scope,
            lash_sansio::llm::types::LlmRequestScope {
                attempt: None,
                ..recorded.scope
            }
        );
        assert_eq!(admitted.contract, recorded.contract);
        let loaded = admitted.prompt.expect("the admitted call has a snapshot");
        assert_eq!(loaded.snapshot, prompt.snapshot);
        assert_eq!(loaded.texts, prompt.texts);
        let finals = |placement| {
            loaded
                .snapshot
                .sections
                .iter()
                .filter(|section| section.placement == placement)
                .filter_map(|section| loaded.text(&section.value))
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        assert_eq!(
            Some(finals(PromptPlacement::InitialInstructions)),
            prompt.initial_instructions
        );
        assert_eq!(
            Some(finals(PromptPlacement::CurrentContext)),
            prompt.current_context
        );
        let intro = &loaded.snapshot.sections[0];
        assert_eq!(loaded.text(&intro.base), Some("protocol intro"));
        assert_eq!(
            loaded.text(&intro.wraps[0].output),
            Some("protocol intro (with memory)")
        );
    }
    assert_eq!(
        runs.load(Ordering::SeqCst),
        6,
        "reading a snapshot back runs no renderer or wrapper"
    );

    let shared = PromptTextRef::of("protocol intro (with memory)")
        .blob
        .as_str()
        .to_owned();
    for call in [1, 2] {
        let row = lash_durable::DurableReads::prompt_snapshot(&reader, &call_key(call))
            .await
            .expect("read the root")
            .expect("the root is retained");
        assert!(
            row.texts.contains(&shared),
            "call {call} roots the unchanged text"
        );
    }
    assert_eq!(
        lash_durable::DurableReads::prompt_texts(&reader, std::slice::from_ref(&shared))
            .await
            .expect("read the shared text")
            .len(),
        1,
        "the unchanged text is stored once"
    );
}
