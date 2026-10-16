use lash_provider_fixtures::runtime_providers::*;

/// The content oracle's own wire decoder reads back exactly the text and
/// usage each provider's encoding wrote, so a ledger mismatch is lash's.
#[test]
fn scripted_turn_encodings_decode_to_what_they_report() {
    let usage = ScriptedUsage {
        input_tokens: i64::from(u32::MAX) + 3,
        cache_read_input_tokens: 1 << 40,
        output_tokens: (1 << 53) - 2,
        reasoning_output_tokens: 7,
    };
    let text = "caf\u{e9} \u{0}\r\n\u{1f980}e\u{301}";
    for kind in MIGRATED_RUNTIME_PROVIDER_KINDS {
        let script = runtime_script_for_turn(
            kind,
            &ScriptedTurn {
                text: text.to_string(),
                usage: Some(usage),
            },
        )
        .expect("scripted turn");
        let emitted = crate::content_oracle::emitted_attempt(&script).expect("decodes");
        assert_eq!(emitted.text, text, "{kind}");
        assert!(emitted.completed, "{kind}");
        assert_eq!(
            emitted.usage,
            Some(crate::content_oracle::UsageBuckets {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_input_tokens: usage.cache_read_input_tokens,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: usage.reasoning_output_tokens,
            }),
            "{kind}"
        );
    }
}
