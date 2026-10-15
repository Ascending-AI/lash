//! SNAPSHOT (FIG-5256): an admitted call's prompt snapshot reads back from
//! the store with every base, wrapper and final text byte for byte, and no
//! renderer or wrapper runs to produce it. Unchanged text is shared across
//! calls. AUDIT (FIG-5946): the snapshot reads back the same once the call
//! settled, which released its request.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_durable::domain::{ModelCallId, PromptCallKey};
use lash_durable::{ActorKey, CommitLabel, FormatSet, MailTx, NodeId, NodeSpec};

use crate::plugin::prompt::{
    OfferedTools, ProjectedHistoryStats, PromptCall, PromptCatalog, PromptInput, PromptModel,
    PromptPlacement, PromptPlan, PromptPurpose, PromptRenderError, PromptSectionId,
    PromptSectionKey, PromptSectionSpec, PromptTextRef, PromptWrapKey, PromptWrapSpec,
    PromptWrapTarget, SectionText, admission_record, load_admitted_call, load_prompt_snapshot,
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
        config: crate::OwnerConfigView::session(crate::AdmittedPluginConfig::default()),
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
                let intro = Arc::clone(&intro);
                reg.prompt().section(
                    PromptSectionSpec::new(key("intro"), PromptPlacement::InitialInstructions),
                    move |_, _| Ok(intro.clone()),
                )
            }),
        ),
        (
            "memory",
            Box::new(move |reg| {
                let note = Arc::clone(&note);
                let wrap = Arc::clone(&wrap);
                reg.prompt().section(
                    PromptSectionSpec::new(key("note"), PromptPlacement::CurrentContext),
                    move |_, _| Ok(note.clone()),
                )?;
                reg.prompt().wrap(
                    PromptWrapSpec::new(
                        PromptWrapKey::new("intro").expect("valid wrap key"),
                        PromptSectionId::new("lash.protocol", key("intro")),
                    ),
                    move |_, _| Ok(wrap.clone()),
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
            serves: Vec::new(),
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
        let loaded = load_prompt_snapshot(&reader, &call_key(call))
            .await
            .expect("the snapshot loads")
            .expect("the admitted call has a snapshot");
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
            row.snapshot_texts.contains(&shared),
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

    // The turn settles both calls: their requests go, their snapshots stay.
    let before = [
        load_prompt_snapshot(&reader, &call_key(1))
            .await
            .expect("load"),
        load_prompt_snapshot(&reader, &call_key(2))
            .await
            .expect("load"),
    ];
    let request_texts = lash_durable::DurableReads::prompt_snapshot(&reader, &call_key(1))
        .await
        .expect("read the root")
        .expect("the root is retained")
        .request_texts;
    let mut tx = lash_durable::DurableStore::begin(&durable, &actor, claimed[0].epoch)
        .await
        .expect("begin the settlement");
    tx.write(lash_durable::DomainWrite::Prompt(
        lash_durable::domain::PromptWrite::Settle {
            session: SessionId::from(SESSION),
            run: lash_sansio::TurnId::from("prompt-turn"),
            open: None,
        },
    ));
    lash_durable::DurableStore::commit(&durable, tx, CommitLabel::new("model.done"))
        .await
        .expect("settle both calls");
    for call in [1, 2] {
        assert!(
            load_admitted_call(&reader, &call_key(call))
                .await
                .expect("the settled call reads")
                .is_none(),
            "call {call}'s request is released with its settlement"
        );
        assert_eq!(
            load_prompt_snapshot(&reader, &call_key(call))
                .await
                .expect("the snapshot loads"),
            before[call as usize - 1],
            "call {call}'s snapshot reads back the same once it settled"
        );
    }
    let sections: std::collections::BTreeSet<String> = composed
        .iter()
        .flat_map(|prompt| prompt.texts.keys().map(|hash| hash.as_str().to_owned()))
        .collect();
    let left: Vec<_> = lash_durable::DurableReads::prompt_texts(&reader, &request_texts)
        .await
        .expect("read the request texts")
        .into_iter()
        .filter(|text| !sections.contains(&text.hash))
        .collect();
    assert!(left.is_empty(), "a settled request's chunks stay: {left:?}");
}

/// FRAGMENT-ONCE (FIG-6398): admission carries the builder's edges without
/// decoding fresh fragments. The admission image pins exact bytes, digests,
/// edges and recorded configuration across the allocation-only cutover.
#[test]
fn prompt_admission_never_decodes_fresh_fragments() {
    use lash_core_store::prompt_sections::RequestFragment;
    use lash_sansio::llm::types::RecordedRequestTemplate;
    use sha2::{Digest as _, Sha256};

    let route = template(1).route.clone();
    let messages: Vec<_> = (0..40)
        .map(|n| serde_json::json!({"role": "user", "content": format!("{n}:{}", "é\n\"\\".repeat(1024))}))
        .collect();
    let grouped = serde_json::json!({"messages": messages, "stream": true}).to_string();
    let rendered =
        serde_json::json!({"input": [{"role": "user", "content": "é\n\"\\".repeat(16384)}]})
            .to_string();
    let templates = [
        template(1),
        template(2),
        RecordedRequestTemplate::literal(route.clone(), true, None, grouped).unwrap(),
        RecordedRequestTemplate::literal(route, false, None, rendered).unwrap(),
    ];
    let mut images = Vec::new();
    let before = RequestFragment::decodes_on_current_thread();
    for template in &templates {
        let record = admission_record(
            call_key(2),
            None,
            template,
            &response(2),
            Some(lash_durable::DurableInstant(42)),
        )
        .unwrap();
        let lash_durable::DomainWrite::Prompt(lash_durable::domain::PromptWrite::Record {
            snapshot,
            request,
            snapshot_texts,
            request_texts,
            prefix,
            ..
        }) = record
        else {
            panic!("an admission is a prompt record")
        };
        let texts = |texts: Vec<lash_durable::domain::PromptText>| {
            texts
                .into_iter()
                .map(|text| (text.hash, text.text, text.children))
                .collect::<Vec<_>>()
        };
        let image = serde_json::to_vec(&(
            snapshot,
            request,
            texts(snapshot_texts),
            texts(request_texts),
            prefix,
        ))
        .unwrap();
        images.push(format!("{:x}", Sha256::digest(image)));
    }
    let decoded = RequestFragment::decodes_on_current_thread() - before;
    println!("admission images: {images:?}; fresh fragment decodes: {decoded}");
    // Captured on the unfixed builder: no stored bytes or edges may change.
    assert_eq!(
        images,
        [
            "affa33b856297ae84a182d9666c37f3a98f561a5c67033631f92f02411713360",
            "d48aaa912847120566ee9aca080b109cb5fff0b48e4232d82250452350377da9",
            "d5393169c2340fbfb2abe00491f2c5e683458fa56e62840604940dc9ff1a7ddd",
            "96a37eb2a0911fae9d97393824b7ce28b8ddcadceeb4202126287656654e958e",
        ]
    );
    assert_eq!(
        decoded, 0,
        "admission must never decode fragments it just encoded"
    );

    // A positive control prevents an inactive observation hook from passing.
    let _: RequestFragment =
        serde_json::from_str(r#"{"fragment":"literal","text":"control"}"#).unwrap();
    assert_eq!(RequestFragment::decodes_on_current_thread() - before, 1);
}
