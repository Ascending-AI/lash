use super::*;

fn id(owner: &str, key: &str) -> PromptSectionId {
    PromptSectionId::new(owner, PromptSectionKey::new(key).expect("valid key"))
}

#[test]
fn a_prompt_key_outside_its_alphabet_does_not_decode() {
    for refused in ["", "Upper", "-lead", "has/slash", &"k".repeat(65)] {
        assert!(PromptSectionKey::new(refused).is_err(), "{refused:?}");
        assert!(serde_json::from_value::<PromptWrapKey>(serde_json::json!(refused)).is_err());
    }
    assert_eq!(
        PromptSectionKey::new("current.memory_v2").unwrap().as_str(),
        "current.memory_v2"
    );
}

#[test]
fn a_plan_orders_and_places_each_section_once() {
    let memory = id("optmem", "current");
    let ordered_twice = PromptPlan {
        order: vec![memory.clone(), memory.clone()],
        ..PromptPlan::default()
    };
    assert_eq!(
        ordered_twice.validate(),
        Err(PromptPlanError::DuplicateOrder {
            section: memory.clone()
        })
    );
    let placed_twice = PromptPlan {
        placements: vec![
            PromptSectionPlacement {
                section: memory.clone(),
                placement: PromptPlacement::CurrentContext,
            },
            PromptSectionPlacement {
                section: memory.clone(),
                placement: PromptPlacement::InitialInstructions,
            },
        ],
        ..PromptPlan::default()
    };
    assert_eq!(
        placed_twice.validate(),
        Err(PromptPlanError::DuplicatePlacement { section: memory })
    );
    let inverted = PromptPlan {
        limits: PromptLimits {
            max_section_bytes: NonZeroU32::new(10).unwrap(),
            max_total_bytes: NonZeroU32::new(9).unwrap(),
            ..PromptLimits::DEFAULT
        },
        ..PromptPlan::default()
    };
    assert!(matches!(
        inverted.validate(),
        Err(PromptPlanError::SectionLimitAboveTotal { .. })
    ));
    assert_eq!(PromptPlan::default().validate(), Ok(()));
}

#[test]
fn a_prompt_snapshot_decodes_only_at_version_one() {
    let section = id("lash.standard", "intro");
    let snapshot = PromptSnapshot {
        version: PromptSnapshotVersion,
        plan: ResolvedPromptPlan {
            purpose: PromptPurpose::Turn,
            sections: vec![ResolvedPromptSection {
                section: section.clone(),
                owner: String::from("lash.standard"),
                placement: PromptPlacement::InitialInstructions,
                placement_source: PlacementSource::PluginDefault,
                wraps: Vec::new(),
            }],
            absent_targets: Vec::new(),
            absent_overrides: Vec::new(),
            limits: PromptLimits::DEFAULT,
        },
        sections: vec![RenderedPromptSection {
            section,
            placement: PromptPlacement::InitialInstructions,
            base: RecordedSectionText::Text {
                text: PromptTextRef::of("You are helpful."),
            },
            wraps: Vec::new(),
            value: RecordedSectionText::Omitted,
        }],
    };
    let mut encoded = serde_json::to_value(&snapshot).expect("snapshot encodes");
    assert_eq!(encoded["version"], 1);
    assert_eq!(
        serde_json::from_value::<PromptSnapshot>(encoded.clone()).expect("version 1 decodes"),
        snapshot
    );
    encoded["version"] = serde_json::json!(2);
    assert!(serde_json::from_value::<PromptSnapshot>(encoded).is_err());
}

#[test]
fn chunked_response_mode_is_recorded_only_outside_the_body() {
    use lash_protocol_types::{
        llm::types::LiveRequestBody, llm::types::ProviderRouteIdentity,
        llm::types::RecordedRequestTemplate,
    };

    let route = ProviderRouteIdentity {
        provider: "fixture".into(),
        endpoint: "https://fixture.test".into(),
        model: "model".into(),
    };
    for (body, outside) in [(" { \"stream\" : true } ", None), ("{}", Some(true))] {
        let template = RecordedRequestTemplate::literal(route.clone(), true, None, body).unwrap();
        let (chunked, texts) = ChunkedRequestTemplate::chunk(&template);
        assert_eq!(chunked.transport_stream, outside);
        let encoded = serde_json::to_value(&chunked).unwrap();
        assert!(encoded.get("stream").is_none());
        let mut decoded: ChunkedRequestTemplate = serde_json::from_value(encoded).unwrap();
        let read = |reference: &BlobRef| texts.get(reference.as_str()).map(|text| &*text.text);
        let assembled = decoded.assemble(read).unwrap();
        assert_eq!(assembled, template);
        let live = LiveRequestBody::fill(std::sync::Arc::new(assembled), vec![]).unwrap();
        assert_eq!(live.to_wire_string(), body);
        assert!(live.stream());
        decoded.transport_stream = if outside.is_none() { Some(false) } else { None };
        assert!(decoded.assemble(read).is_err());
    }
}

/// LO-PREFIX: retained source rendering, canonical lowering and fragment
/// preparation visit the changed suffix while matching a full cold reference.
#[test]
fn lo_prefix_tail_work_matches_full_render() {
    use crate::history_growth_law::allocation_counter::Measurement;
    use lash_protocol_types::llm::types::{
        LlmRequest, LlmRequestScope, LlmToolChoice, ProviderRouteIdentity, RecordedRequestTemplate,
        RetainedLlmRequest, SegmentCache,
    };
    use lash_protocol_types::session_model::message::{BaseRenderCache, HistoryPresentation};
    use std::sync::Arc;

    let message = |index: usize| crate::Message {
        id: format!("message-{index}"),
        role: crate::MessageRole::User,
        parts: crate::shared_parts(vec![crate::Part::text(
            format!("part-{index}"),
            format!("body {index:04}"),
            None,
        )]),
        origin: None,
        reply_marker: None,
    };
    let envelope = || {
        LlmRequest {
        instructions: None,
        model: crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(crate::LlmProfileKey::new("prefix"), lash_protocol_types::llm_profile::LlmProfileMetadata::builder("prefix").context_window_tokens(32000).cache_retention(lash_protocol_types::llm::capability::CacheRetention::Short).build().unwrap())),
        messages: Vec::new(), tools: Arc::new(Vec::new()), tool_choice: LlmToolChoice::None,
        attachment_acceptance: Arc::new(lash_protocol_types::llm::types::AttachmentCapabilitySnapshot {
            revision:"prefix".into(),acceptors:vec![lash_protocol_types::llm::types::AttachmentAcceptor {provider:"prefix".into(),rules:vec![lash_protocol_types::llm::types::AttachmentAcceptanceRule {positions:vec![lash_protocol_types::llm::attachment_delivery::AttachmentPosition::Message],media_types:vec!["image/png".into()],media_families:vec![],forms:lash_protocol_types::llm::attachment_delivery::DeliveryForms {bytes:true,url:true,provider_file:false}}]}]
        }), generation: Default::default(),
        scope: LlmRequestScope::new("prefix","frame","call"), output_spec: None,
        stream_events: None, provider_trace: None, attempt_control: Default::default(),
    }
    };
    let renders = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = Arc::clone(&renders);
    let policy = HistoryPresentation::new(move |_, part| {
        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        part.content().into_owned()
    });

    let route = ProviderRouteIdentity::new("prefix", "prefix", "prefix");
    // Linking one prefix per leaf bounds publication over the whole run.
    // Rebuilding a spine would exceed that bound.
    {
        let mut owner = OwnerPromptState::default();
        let mut request = RetainedLlmRequest::from_request(envelope());
        let lowering = SegmentCache::default();
        let mut branches = 0;
        for index in 0..128 {
            request = request.with_messages(request.messages().append(&[
                lash_protocol_types::llm::types::LlmMessage::text(
                    lash_protocol_types::llm::types::LlmRole::User,
                    format!("append body {index}"),
                ),
            ]));
            let template =
                RecordedRequestTemplate::of_retained_request(route.clone(), &request, &lowering)
                    .unwrap();
            let (_, texts) = owner.chunk(&template);
            branches += texts
                .supplied_delta()
                .unwrap()
                .filter(|text| {
                    matches!(
                        serde_json::from_str::<RequestFragment>(&text.text).unwrap(),
                        RequestFragment::Branch { .. }
                    )
                })
                .count();
        }
        assert!(
            branches < owner.retained_leaves(),
            "append publication must stay below one link per leaf: {branches} links / {} leaves",
            owner.retained_leaves()
        );
    }
    // Retained bodies put immutable configuration before messages. Compare
    // request values, exact attachment slots and all transport metadata.
    let canonical = |template: &RecordedRequestTemplate| {
        (
            template.fetch_horizon,
            template.route.clone(),
            template.stream(),
            template.transport_stream(),
            template.generation,
            template.wire_features.clone(),
            template.slots().cloned().collect::<Vec<_>>(),
            serde_json::to_value(template.canonical_request().unwrap()).unwrap(),
        )
    };
    let measured = [10, 100, 1000].map(|depth| {
        let base: lash_protocol_types::AppendVec<_> =
            (0..depth).map(message).collect::<Vec<_>>().into();
        let cache = Arc::new(BaseRenderCache::new());
        let source =
            crate::MessageSequence::from_base(base.clone()).with_base_render_cache(cache.clone());
        let request = RetainedLlmRequest::from_request(envelope())
            .with_messages(source.render_retained(&policy));
        // The public driver seam must share this carrier. Adapting through
        // Arc<LlmRequest> here would copy the whole prompt on every call.
        let sharing = Measurement::start();
        let shared = request.clone();
        let shared_cost = sharing.finish();
        assert_eq!(
            shared_cost.allocations, 0,
            "request handoff must share its history"
        );
        let flattening = Measurement::start();
        let flat = shared.materialize();
        let flat_cost = flattening.finish();
        assert_eq!(flat.messages.len(), depth);
        eprintln!(
            "driver request boundary depth={depth}: retained={shared_cost:?} flat={flat_cost:?}"
        );
        let lowering = SegmentCache::default();
        let template =
            RecordedRequestTemplate::of_retained_request(route.clone(), &request, &lowering)
                .unwrap();
        let mut owner = OwnerPromptState::default();
        let (old, held) = owner.chunk(&template);
        let original_template = template.clone();
        let prior = owner.clone();
        let next_source = crate::MessageSequence::from_base_and_delta(base, vec![message(depth)])
            .with_base_render_cache(cache);
        renders.store(0, std::sync::atomic::Ordering::Relaxed);
        let decodes = RequestFragment::decodes_on_current_thread();
        let measurement = Measurement::start();
        let next = request.with_messages(next_source.render_retained(&policy));
        let template =
            RecordedRequestTemplate::of_retained_request(route.clone(), &next, &lowering).unwrap();
        let (chunked, texts) = owner.chunk(&template);
        let counts = measurement.finish();
        assert_eq!(
            RequestFragment::decodes_on_current_thread(),
            decodes,
            "append decodes no retained prefix"
        );
        assert_eq!(
            renders.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "an append renders no retained prefix part"
        );
        assert_eq!(request.messages().shared_prefix(next.messages()), depth);
        let mut digest = lash_protocol_types::core_support::Blake3DomainHasher::new(
            "lash-llm-request-content/v1",
        );
        digest.update(serde_json::to_vec(&next.materialize().messages).unwrap());
        assert_eq!(
            next.messages().content_digest(),
            digest.finalize_hex(),
            "retained digest preserves the journal encoding"
        );
        let reference = RecordedRequestTemplate::of_request(
            route.clone(),
            &next.materialize(),
            &SegmentCache::disabled(),
        )
        .unwrap();
        let assembled = chunked
            .assemble(|hash| texts.get(hash.as_str()).map(|text| &*text.text))
            .unwrap();
        assert_eq!(assembled, template);
        assert_eq!(canonical(&assembled), canonical(&reference));
        let assembled = old
            .assemble(|hash| held.get(hash.as_str()).map(|text| &*text.text))
            .unwrap();
        assert_eq!(assembled, original_template);
        let reference = RecordedRequestTemplate::of_request(
            route.clone(),
            &request.materialize(),
            &SegmentCache::disabled(),
        )
        .unwrap();
        assert_eq!(canonical(&assembled), canonical(&reference));
        let mut changed = next.messages().get(depth / 2).unwrap().clone();
        changed.role = lash_protocol_types::llm::types::LlmRole::Assistant;
        changed.starts_user_segment = true;
        let mut blocks = changed.blocks.as_ref().clone();
        if let lash_protocol_types::llm::types::LlmContentBlock::Text {
            cache_breakpoint, ..
        } = &mut blocks[0]
        {
            *cache_breakpoint = true;
        }
        changed.blocks = Arc::new(blocks);
        let reference = lash_protocol_types::AttachmentRef {
            id: lash_protocol_types::AttachmentId::parse("ab".repeat(32)).unwrap(),
            media_type: lash_protocol_types::MediaType::parse("image/png").unwrap(),
            byte_len: 4,
            type_metadata: None,
            label: None,
        };
        let attachment = lash_protocol_types::llm::types::LlmContentBlock::Attachment {
            reference: Box::new(reference),
        };
        let duplicates = lash_protocol_types::llm::types::LlmMessage::new(
            lash_protocol_types::llm::types::LlmRole::User,
            vec![attachment.clone(), attachment],
        );
        let edited = next
            .messages()
            .prefix(depth / 2)
            .append(&[changed, duplicates]);
        for rewrite in [
            edited,
            next.messages().prefix(4),
            next.messages()
                .prefix(3)
                .append(&[lash_protocol_types::llm::types::LlmMessage::text(
                    lash_protocol_types::llm::types::LlmRole::Assistant,
                    "rewrite",
                )]),
        ] {
            let rewritten = next.with_messages(rewrite);
            let template =
                RecordedRequestTemplate::of_retained_request(route.clone(), &rewritten, &lowering)
                    .unwrap();
            let (chunked, texts) = owner.chunk(&template);
            let reference = RecordedRequestTemplate::of_request(
                route.clone(),
                &rewritten.materialize(),
                &SegmentCache::disabled(),
            )
            .unwrap();
            assert_eq!(canonical(&template), canonical(&reference));
            assert_eq!(
                chunked
                    .assemble(|hash| texts.get(hash.as_str()).map(|text| &*text.text))
                    .unwrap(),
                template
            );
            let mut digest = lash_protocol_types::core_support::Blake3DomainHasher::new(
                "lash-llm-request-content/v1",
            );
            digest.update(serde_json::to_vec(&rewritten.materialize().messages).unwrap());
            assert_eq!(rewritten.messages().content_digest(), digest.finalize_hex());
        }
        let different = HistoryPresentation::new(|_, part| format!("different {}", part.content()));
        let other = next.with_messages(source.render_retained(&different));
        let lowered =
            RecordedRequestTemplate::of_retained_request(route.clone(), &other, &lowering).unwrap();
        let (chunked, texts) = owner.chunk(&lowered);
        assert_eq!(
            chunked
                .assemble(|hash| texts.get(hash.as_str()).map(|text| &*text.text))
                .unwrap(),
            lowered
        );
        drop(prior);
        counts
    });
    eprintln!("LO-PREFIX allocation at 10/100/1000: {measured:?}");
    assert!(
        measured[2].allocated <= measured[0].allocated + 32768,
        "prefix copies must be bounded by changed paths: {measured:?}"
    );
    assert!(
        measured[2].allocations <= measured[0].allocations + 96,
        "prefix visits must not allocate per retained message: {measured:?}"
    );
}

/// FIG-7382: a fresh rendering of every prior turn must not retain a separate
/// source, lowered body and decoded message population for every send.
#[test]
fn rerendered_frame_retains_only_one_copy_of_unchanged_messages() {
    use crate::history_growth_law::allocation_counter::Measurement;
    use lash_protocol_types::llm::types::{
        LlmMessage, LlmRequest, LlmRequestScope, LlmRole, LlmToolChoice, ProviderRouteIdentity,
        RecordedRequestTemplate, RetainedLlmRequest,
    };
    use std::sync::Arc;

    let envelope = LlmRequest {
        instructions: None,
        model: crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(
            crate::LlmProfileKey::new("frame-memory"),
            lash_protocol_types::llm_profile::LlmProfileMetadata::builder("frame-memory")
                .context_window_tokens(32000)
                .cache_retention(lash_protocol_types::llm::capability::CacheRetention::Short)
                .build()
                .unwrap(),
        )),
        messages: Vec::new(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::None,
        attachment_acceptance: Arc::default(),
        generation: Default::default(),
        scope: LlmRequestScope::new("frame-memory", "frame", "call"),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
        attempt_control: Default::default(),
    };
    let route = ProviderRouteIdentity::new("frame-memory", "frame-memory", "frame-memory");
    let mut owner = OwnerPromptState::default();
    let mut appended_bytes = 0;
    let measurement = Measurement::start();
    for turn in 0..256 {
        let mut draft = owner.draft();
        // Like RLM's chronological renderer, each call reconstructs its values.
        let messages: Vec<_> = (0..=turn)
            .flat_map(|prior| {
                [
                    LlmMessage::text(LlmRole::User, format!("history turn {prior}")),
                    LlmMessage::text(LlmRole::Assistant, format!("checked total {prior}")),
                ]
            })
            .chain([LlmMessage::text(LlmRole::User, format!("iteration {turn}"))])
            .collect();
        appended_bytes += serde_json::to_vec(&messages[messages.len() - 3..messages.len() - 1])
            .unwrap()
            .len();
        let mut flat = envelope.clone();
        flat.messages = messages;
        let request = RetainedLlmRequest::from_request(flat);
        let shared = draft.retain_request(&request);
        let template = RecordedRequestTemplate::of_retained_request(
            route.clone(),
            &shared,
            draft.lowering_cache(),
        )
        .unwrap();
        // Sending decodes canonical items and memoizes them beside their segments.
        let sent = template.canonical_request().unwrap();
        assert_eq!(
            serde_json::to_vec(&sent.messages).unwrap(),
            serde_json::to_vec(request.messages()).unwrap()
        );
        draft.chunk(&template);
        owner = draft;
    }
    let counts = measurement.finish();
    let live = counts.allocated.saturating_sub(counts.deallocated);
    eprintln!("256 scripted turns: retained={live}, appended={appended_bytes}");
    assert!(
        live <= appended_bytes + 8 * 1024 * 1024,
        "unchanged rendered turns must be retained once: {live} live bytes, {appended_bytes} appended bytes"
    );
}
