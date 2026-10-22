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

// Independent full rebuild of the canonical fragment forest.
fn rebuild_fragments(
    template: &lash_protocol_types::llm::types::RecordedRequestTemplate,
) -> (
    ChunkedRequestTemplate,
    Vec<(BlobRef, std::sync::Arc<AdmittedPromptText>)>,
) {
    let mut leaves: Vec<Leaf> = Vec::new();
    let mut envelope = TextChunker::new(true);
    let mut items_end = None;
    let mut previous_item = false;
    for segment in template.segments() {
        if segment.is_item() {
            let kept =
                item_leaves_owned(segment, previous_item, &AllocationBudget::unowned()).unwrap();
            leaves.extend(kept);
            items_end = Some(leaves.len());
            previous_item = true;
            continue;
        }
        previous_item = false;
        for part in segment.parts() {
            match part {
                lash_protocol_types::llm::types::SegmentPart::Literal(text) => {
                    envelope
                        .literal_owned(text, &mut leaves, &AllocationBudget::unowned())
                        .unwrap();
                }
                lash_protocol_types::llm::types::SegmentPart::Slot(slot) => {
                    leaves.push(encode_fragment(&RequestFragment::Attachment {
                        slot: slot.as_ref(),
                    }));
                }
            }
        }
    }
    let prefix_end = items_end.or(envelope.prefix_end);
    let mut fragments = Vec::with_capacity(leaves.len());
    let mut texts = Vec::with_capacity(leaves.len() * 2);
    for (reference, encoded) in leaves {
        texts.push((reference.blob.clone(), encoded));
        fragments.push(reference);
    }
    let prefix_start = texts.len();
    let prefix = prefix_end
        .filter(|end| *end > 0 && *end < fragments.len())
        .map(|end| {
            let mut rest = &fragments[..end];
            let mut children = Vec::new();
            while !rest.is_empty() {
                let size = 1usize << rest.len().ilog2();
                children.push(fragment_tree(&rest[..size], &mut texts));
                rest = &rest[size..];
            }
            keep_fragment(&RequestFragment::Index { children }, &mut texts)
        });
    let prefix_hashes: std::collections::BTreeSet<_> = prefix_end
        .filter(|_| prefix.is_some())
        .into_iter()
        .flat_map(|end| {
            fragments[..end]
                .iter()
                .map(|reference| reference.blob.clone())
        })
        .chain(texts[prefix_start..].iter().map(|(hash, _)| hash.clone()))
        .collect();
    let root = if let (Some(prefix), Some(end)) = (&prefix, prefix_end) {
        let right = fragment_tree(&fragments[end..], &mut texts);
        keep_fragment(
            &RequestFragment::Branch {
                left: prefix.clone(),
                right,
            },
            &mut texts,
        )
    } else {
        fragment_tree(&fragments, &mut texts)
    };
    let (texts, inline) = if prefix.is_some() {
        texts
            .into_iter()
            .partition(|(hash, _)| prefix_hashes.contains(hash))
    } else {
        (texts, Vec::new())
    };
    (
        ChunkedRequestTemplate {
            fetch_horizon: template.fetch_horizon,
            route: template.route.clone(),
            transport_stream: template.transport_stream(),
            generation: template.generation,
            wire_features: template.wire_features.clone(),
            root,
            prefix,
            inline: inline
                .into_iter()
                .map(|(hash, fragment)| (hash, fragment.text.to_string()))
                .collect(),
        },
        texts,
    )
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
        messages: Vec::new(), tools: Arc::new((Vec::new()).into()), tool_choice: LlmToolChoice::None,
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
    let budget = lash_protocol_types::allocation::AllocationBudget::unowned();
    let route = ProviderRouteIdentity::new("prefix", "prefix", "prefix");
    // Binary carries create fewer than one complete branch per appended
    // leaf over the whole run. Rebuilding an incomplete spine violates this
    // bound even though each individual append only walks a short path.
    {
        let mut owner = OwnerPromptState::default();
        let mut request = RetainedLlmRequest::from_request(envelope());
        let lowering = SegmentCache::default();
        let mut branches = 0;
        for index in 0..128 {
            request = request.with_messages(
                request
                    .messages()
                    .append(
                        &[lash_protocol_types::llm::types::LlmMessage::text(
                            lash_protocol_types::llm::types::LlmRole::User,
                            format!("carry body {index}"),
                        )],
                        &budget,
                    )
                    .unwrap(),
            );
            let template = RecordedRequestTemplate::of_retained_request(
                route.clone(),
                &request,
                &lowering,
                &budget,
            )
            .unwrap();
            let (_, texts) = owner.chunk(&template, &budget).unwrap();
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
            "append branch work must amortize below one per leaf: {branches} branches / {} leaves",
            owner.retained_leaves()
        );
    }
    let measured = [10, 100, 1000].map(|depth| {
        let base: lash_protocol_types::AppendVec<_> =
            (0..depth).map(message).collect::<Vec<_>>().into();
        let cache = Arc::new(BaseRenderCache::new());
        let source =
            crate::MessageSequence::from_base(base.clone()).with_base_render_cache(cache.clone());
        let request = RetainedLlmRequest::from_request(envelope())
            .with_messages(source.render_retained(&policy, &budget).unwrap());
        let lowering = SegmentCache::default();
        let template = RecordedRequestTemplate::of_retained_request(
            route.clone(),
            &request,
            &lowering,
            &budget,
        )
        .unwrap();
        let mut owner = OwnerPromptState::default();
        let (old, held) = owner.chunk(&template, &budget).unwrap();
        let prior = owner.clone();
        let next_source = crate::MessageSequence::from_base_and_delta(base, vec![message(depth)])
            .unwrap()
            .with_base_render_cache(cache);
        renders.store(0, std::sync::atomic::Ordering::Relaxed);
        let decodes = RequestFragment::decodes_on_current_thread();
        let measurement = Measurement::start();
        let next = request.with_messages(next_source.render_retained(&policy, &budget).unwrap());
        let template =
            RecordedRequestTemplate::of_retained_request(route.clone(), &next, &lowering, &budget)
                .unwrap();
        let (chunked, texts) = owner.chunk(&template, &budget).unwrap();
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
        let (expected, expected_texts) = rebuild_fragments(&reference);
        assert_eq!(
            serde_json::to_vec(&chunked).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        assert_eq!(
            texts,
            RequestTexts::from_texts(
                expected_texts
                    .into_iter()
                    .map(|(_, text)| text.body.clone())
            )
        );
        assert_eq!(
            old.assemble(|hash| held.get(hash.as_str()).map(|text| &*text.text))
                .unwrap(),
            RecordedRequestTemplate::of_request(
                route.clone(),
                &request.materialize(),
                &SegmentCache::disabled()
            )
            .unwrap()
        );
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
            .prefix(depth / 2, &budget)
            .unwrap()
            .append(&[changed, duplicates], &budget)
            .unwrap();
        for rewrite in [
            edited,
            next.messages().prefix(4, &budget).unwrap(),
            next.messages()
                .prefix(3, &budget)
                .unwrap()
                .append(
                    &[lash_protocol_types::llm::types::LlmMessage::text(
                        lash_protocol_types::llm::types::LlmRole::Assistant,
                        "rewrite",
                    )],
                    &budget,
                )
                .unwrap(),
        ] {
            let rewritten = next.with_messages(rewrite);
            let template = RecordedRequestTemplate::of_retained_request(
                route.clone(),
                &rewritten,
                &lowering,
                &budget,
            )
            .unwrap();
            let (chunked, texts) = owner.chunk(&template, &budget).unwrap();
            let reference = RecordedRequestTemplate::of_request(
                route.clone(),
                &rewritten.materialize(),
                &SegmentCache::disabled(),
            )
            .unwrap();
            assert_eq!(template, reference);
            let (expected, expected_texts) = rebuild_fragments(&reference);
            assert_eq!(
                serde_json::to_vec(&chunked).unwrap(),
                serde_json::to_vec(&expected).unwrap()
            );
            assert_eq!(
                texts,
                RequestTexts::from_texts(
                    expected_texts
                        .into_iter()
                        .map(|(_, text)| text.body.clone())
                )
            );
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
        let other = next.with_messages(source.render_retained(&different, &budget).unwrap());
        let lowered =
            RecordedRequestTemplate::of_retained_request(route.clone(), &other, &lowering, &budget)
                .unwrap();
        let (chunked, texts) = owner.chunk(&lowered, &budget).unwrap();
        assert_eq!(
            chunked
                .assemble(|hash| texts.get(hash.as_str()).map(|text| &*text.text))
                .unwrap(),
            lowered
        );
        drop(prior);
        counts
    });
    // Refusal is a typed, atomic draft failure. Every reservation cut must
    // leave the acknowledged tree and its charges unchanged; a held reader
    // keeps its allocations charged through owner invalidation and final drop.
    {
        use lash_protocol_types::allocation::{
            AllocationBudget, AllocationError, AllocationHold, AllocationLedger,
            AllocationReservation, ReserveError,
        };
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
        #[derive(Default)]
        struct Ledger {
            used: AtomicU64,
            calls: AtomicUsize,
            refuse: AtomicUsize,
            kind: AtomicUsize,
        }
        struct Token {
            ledger: Arc<Ledger>,
            bytes: u64,
        }
        impl AllocationReservation for Token {
            fn retain(&self) {}
        }
        impl Drop for Token {
            fn drop(&mut self) {
                self.ledger.used.fetch_sub(self.bytes, Ordering::SeqCst);
            }
        }
        struct LedgerHandle(Arc<Ledger>);
        impl AllocationLedger for LedgerHandle {
            fn reserve(&self, bytes: u64) -> Result<AllocationHold, AllocationError> {
                let ordinal = self.0.calls.fetch_add(1, Ordering::SeqCst);
                if ordinal >= self.0.refuse.load(Ordering::SeqCst) {
                    return Err(AllocationError {
                        kind: if self.0.kind.load(Ordering::SeqCst) == 0 {
                            ReserveError::Pressure
                        } else {
                            ReserveError::TooLarge
                        },
                        actor: "prefix-law".into(),
                        requested: bytes,
                        capacity: 0,
                    });
                }
                self.0.used.fetch_add(bytes, Ordering::SeqCst);
                Ok(AllocationHold::new(Token {
                    ledger: self.0.clone(),
                    bytes,
                }))
            }
            fn reserve_shared(
                &self,
                _identity: usize,
                bytes: u64,
            ) -> Result<AllocationHold, AllocationError> {
                self.reserve(bytes)
            }
        }
        let ledger = Arc::new(Ledger::default());
        ledger.refuse.store(usize::MAX, Ordering::SeqCst);
        let budget = AllocationBudget::new(LedgerHandle(ledger.clone()));
        let source = crate::MessageSequence::from_owned((0..10).map(message).collect());
        let request = RetainedLlmRequest::try_from_request(envelope(), &budget)
            .unwrap()
            .with_messages(source.render_retained(&policy, &budget).unwrap());
        let cache = SegmentCache::default();
        let base_template =
            RecordedRequestTemplate::of_retained_request(route.clone(), &request, &cache, &budget)
                .unwrap();
        let mut owner = OwnerPromptState::default();
        let (old, held) = owner.chunk(&base_template, &budget).unwrap();
        let old_root = owner.retained_root().unwrap();
        let next = request.with_messages(
            request
                .messages()
                .append(
                    &[lash_protocol_types::llm::types::LlmMessage::text(
                        lash_protocol_types::llm::types::LlmRole::User,
                        "tail",
                    )],
                    &budget,
                )
                .unwrap(),
        );
        let template =
            RecordedRequestTemplate::of_retained_request(route.clone(), &next, &cache, &budget)
                .unwrap();
        let start = ledger.calls.load(Ordering::SeqCst);
        drop(owner.clone().chunk(&template, &budget).unwrap());
        let cuts = ledger.calls.load(Ordering::SeqCst) - start;
        let acknowledged = ledger.used.load(Ordering::SeqCst);
        for kind in [0, 1] {
            ledger.kind.store(kind, Ordering::SeqCst);
            for cut in 0..cuts {
                ledger
                    .refuse
                    .store(ledger.calls.load(Ordering::SeqCst) + cut, Ordering::SeqCst);
                let error = owner.chunk(&template, &budget).unwrap_err();
                assert_eq!(
                    error.kind,
                    if kind == 0 {
                        ReserveError::Pressure
                    } else {
                        ReserveError::TooLarge
                    }
                );
                assert_eq!(owner.retained_root().unwrap(), old_root);
                assert_eq!(
                    ledger.used.load(Ordering::SeqCst),
                    acknowledged,
                    "failed draft releases its reservations at cut {cut}"
                );
            }
        }
        ledger.refuse.store(usize::MAX, Ordering::SeqCst);
        let mut weak = None;
        held.texts
            .for_each(|_, entry| weak = Some(Arc::downgrade(&entry.text)));
        owner.clear();
        drop((source, request, next, base_template, template, cache));
        assert!(
            weak.as_ref().unwrap().upgrade().is_some(),
            "held old reader owns its body after invalidation"
        );
        assert!(
            ledger.used.load(Ordering::SeqCst) > 0,
            "held bytes remain charged"
        );
        assert!(
            old.assemble(|hash| held.get(hash.as_str()).map(|text| &*text.text))
                .is_ok()
        );
        drop(held);
        assert!(
            weak.unwrap().upgrade().is_none(),
            "last reader releases old bodies"
        );
        assert_eq!(
            ledger.used.load(Ordering::SeqCst),
            0,
            "last reader releases all reservations"
        );
    }
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
