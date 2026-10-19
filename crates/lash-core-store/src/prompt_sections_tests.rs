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

/// PREFIX-DELTA (FIG-5956): a growing wire history relists neither its
/// fragment bytes nor its index prefix. Count newly addressed bodies and
/// the admission's encoded index, independently of backend deduplication.
#[test]
fn growing_requests_submit_new_fragments_and_logarithmic_index_paths() {
    use lash_protocol_types::{
        llm::types::ProviderRouteIdentity, llm::types::RecordedRequestTemplate,
    };
    let route = ProviderRouteIdentity {
        provider: "fixture".into(),
        endpoint: "https://fixture.test".into(),
        model: "model".into(),
    };
    for rendered in [false, true] {
        let mut held = std::collections::BTreeSet::new();
        let mut messages = Vec::new();
        for ordinal in 0..256 {
            messages.push(format!(
                "{{\"role\":\"user\",\"content\":\"{ordinal}:{}\"}}",
                "x".repeat(2048)
            ));
            let wire = if rendered {
                serde_json::json!({"messages": [{"role": "user", "content": messages.join("\n")}], "stream": true}).to_string()
            } else {
                format!("{{\"messages\":[{}],\"stream\":true}}", messages.join(","))
            };
            let template =
                RecordedRequestTemplate::literal(route.clone(), true, None, wire.as_str()).unwrap();
            let (index, texts) = ChunkedRequestTemplate::chunk(&template);
            let fresh: Vec<_> = texts
                .iter()
                .filter(|text| !held.contains(&text.hash))
                .collect();
            let logarithm = (ordinal + 1usize).ilog2() as usize + 1;
            let submitted = fresh.iter().map(|text| text.text.len()).sum::<usize>();
            assert!(
                submitted <= 8192 + 4096 * logarithm,
                "call {ordinal} submitted {submitted} bytes for one new message"
            );
            let metadata = serde_json::to_string(&index).unwrap().len();
            assert!(
                metadata <= 1024,
                "call {ordinal} relisted {metadata} bytes of fragment index"
            );
            let read = |reference: &BlobRef| texts.get(reference.as_str()).map(|text| &*text.text);
            assert_eq!(index.assemble(read).unwrap(), template);
            held.extend(texts.iter().map(|text| text.hash.clone()));
        }
    }
}

/// PREFIX-CARRY: carrying prepared metadata is identical to rebuilding the
/// exact fragment forest, even when compaction keeps the cache's first item.
#[test]
fn carried_prefix_matches_rebuild_across_appends_compaction_and_cache_break() {
    use lash_protocol_types::llm::types::{
        ProviderRouteIdentity, RecordedRequestTemplate, RequestSegment,
    };
    let route = ProviderRouteIdentity {
        provider: "fixture".into(),
        endpoint: "https://fixture.test".into(),
        model: "model".into(),
    };
    let item =
        |text: &str| RequestSegment::item_json(&serde_json::json!({"content": text})).unwrap();
    let slot = lash_protocol_types::llm::types::AttachmentSlot {
        reference: lash_protocol_types::AttachmentRef {
            id: lash_protocol_types::AttachmentId::parse("ab".repeat(32)).unwrap(),
            media_type: lash_protocol_types::MediaType::parse("image/png").unwrap(),
            byte_len: 4,
            type_metadata: None,
            label: None,
        },
        position: lash_protocol_types::llm::attachment_delivery::AttachmentPosition::Message,
        accepts: lash_protocol_types::llm::attachment_delivery::ProviderAccepts {
            bytes: true,
            url: true,
            provider_file: None,
        },
        codec: lash_protocol_types::llm::types::SlotCodec {
            name: "fixture".into(),
            revision: 1,
        },
    };
    let attachment = RequestSegment::from_parts(
        lash_protocol_types::llm::types::SegmentKind::Item,
        vec![lash_protocol_types::llm::types::SegmentPart::Slot(
            Box::new(slot.clone()),
        )],
    )
    .unwrap();
    let mut items = vec![item("retained first item"), attachment];
    for step in 0..24 {
        if step == 10 {
            // Keep the memo owner, but remove the middle of its old tree.
            items.drain(1..8);
        }
        if step == 16 {
            // A provider's cache breakpoint can rewrite an interior item.
            items[1] = item("same history, changed provider cache breakpoint");
        }
        items.push(if step % 3 == 0 {
            item(&"é\n\"\\".repeat(8192))
        } else {
            item("duplicate leaf")
        });
        let mut builder = RecordedRequestTemplate::builder(route.clone(), true, None);
        builder.literal(if step >= 20 {
            "{\"cache\":false,\"messages\":["
        } else {
            "{\"messages\":["
        });
        for item in &items {
            builder.segment(item);
        }
        // This slot is also a prefix leaf: the full rebuild keeps its body
        // with the prefix and omits that duplicate from the inline envelope.
        builder
            .literal("],\"echo\":")
            .attachment(slot.clone())
            .literal(",\"stream\":true}");
        let template = builder.finish().unwrap();
        let (carried, texts) = ChunkedRequestTemplate::chunk(&template);
        let (rebuilt, rebuilt_texts) = rebuild_fragments(&template);
        assert_eq!(
            serde_json::to_vec(&carried).unwrap(),
            serde_json::to_vec(&rebuilt).unwrap(),
            "fragment and tree identities at step {step}"
        );
        let rebuilt_texts = RequestTexts::from_texts(
            rebuilt_texts
                .into_iter()
                .map(|(_, text)| text.as_ref().clone()),
        );
        assert_eq!(texts, rebuilt_texts, "active forest at step {step}");
        // The carried prefix tree's root is the forest's only root: what a
        // full pass over the forest finds (FIG-6889).
        assert_eq!(texts.roots(), rebuilt_texts.roots(), "roots at step {step}");
        assert_eq!(
            carried
                .assemble(|hash| texts.get(hash.as_str()).map(|text| text.text.as_ref()))
                .unwrap(),
            template
        );
    }
}

// Independent full rebuild of the canonical fragment forest.
fn rebuild_fragments(
    template: &lash_protocol_types::llm::types::RecordedRequestTemplate,
) -> (
    ChunkedRequestTemplate,
    Vec<(BlobRef, std::sync::Arc<PromptText>)>,
) {
    let mut leaves: Vec<Leaf> = Vec::new();
    let mut envelope = TextChunker::new(true);
    let mut items_end = None;
    let mut previous_item = false;
    for segment in template.segments() {
        if segment.is_item() {
            let kept = if previous_item {
                segment
                    .derived(|item| SeparatedItemLeaves(item_leaves(item, true)))
                    .0
                    .clone()
            } else {
                segment
                    .derived(|item| ItemLeaves(item_leaves(item, false)))
                    .0
                    .clone()
            };
            leaves.extend(kept);
            items_end = Some(leaves.len());
            previous_item = true;
            continue;
        }
        previous_item = false;
        for part in segment.parts() {
            match part {
                lash_protocol_types::llm::types::SegmentPart::Literal(text) => {
                    envelope.literal(text, &mut leaves);
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
        .map(|end| fragment_tree(&fragments[..end], &mut texts));
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
