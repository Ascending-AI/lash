use crate::request::BreakpointAddress;

#[test]
fn marked_whitespace_only_block_falls_back_without_wire_marker() {
    let provider = AnthropicProvider::new("key");
    let req = request(vec![LlmMessage::new(
        LlmRole::User,
        vec![
            LlmContentBlock::Text {
                text: "  \n\t".into(),
                response_meta: None,
                cache_breakpoint: true,
            },
            LlmContentBlock::Text {
                text: "first surviving block".into(),
                response_meta: None,
                cache_breakpoint: false,
            },
            LlmContentBlock::Text {
                text: "last surviving block".into(),
                response_meta: None,
                cache_breakpoint: false,
            },
        ],
    )]);

    let (_, _, breakpoint) = provider.build_messages(&req);
    assert!(breakpoint.is_none(), "a dropped block has no address");

    let body = provider.build_request_body(&req).expect("body");

    assert!(
        body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert_eq!(
        body["messages"][0]["content"][1]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert_eq!(count_object_key(&body, "__lash_cache_breakpoint"), 0);
}

#[test]
fn marked_block_in_second_same_role_message_keeps_merged_address() {
    let provider = AnthropicProvider::new("key");
    let req = request(vec![
        LlmMessage::text(LlmRole::User, "first block"),
        LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Text {
                text: "marked second block".into(),
                response_meta: None,
                cache_breakpoint: true,
            }],
        ),
    ]);

    let (_, messages, breakpoint) = provider.build_messages(&req);
    assert_eq!(messages.len(), 1, "same-role messages merge on the wire");
    assert_eq!(
        breakpoint,
        Some(BreakpointAddress {
            message_index: 0,
            block_index: 1,
        })
    );

    let body = provider.build_request_body(&req).expect("body");

    assert!(
        body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert_eq!(
        body["messages"][0]["content"][1]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert_eq!(count_object_key(&body, "cache_control"), 1);
    assert_eq!(count_object_key(&body, "__lash_cache_breakpoint"), 0);
}

#[test]
fn marked_first_system_message_collapses_without_wire_marker() {
    let provider = AnthropicProvider::new("key");
    let req = request(vec![
        LlmMessage::new(
            LlmRole::System,
            vec![LlmContentBlock::Text {
                text: "stable system prompt".into(),
                response_meta: None,
                cache_breakpoint: true,
            }],
        ),
        LlmMessage::text(LlmRole::User, "dynamic tail"),
    ]);

    let (_, _, breakpoint) = provider.build_messages(&req);
    assert!(
        breakpoint.is_none(),
        "a block collapsed into the system prompt has no message address"
    );

    let body = provider.build_request_body(&req).expect("body");

    assert_eq!(
        body["system"][0]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert_eq!(count_object_key(&body, "__lash_cache_breakpoint"), 0);
}

#[test]
fn no_retention_omits_cache_control_and_wire_marker_for_marked_block() {
    let provider = AnthropicProvider::new("key").with_options(ProviderOptions {
        cache_retention: CacheRetention::None,
        ..ProviderOptions::default()
    });
    let req = request(vec![LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Text {
            text: "stable history".into(),
            response_meta: None,
            cache_breakpoint: true,
        }],
    )]);

    let body = provider.build_request_body(&req).expect("body");

    assert_eq!(count_object_key(&body, "cache_control"), 0);
    assert_eq!(count_object_key(&body, "__lash_cache_breakpoint"), 0);
}
