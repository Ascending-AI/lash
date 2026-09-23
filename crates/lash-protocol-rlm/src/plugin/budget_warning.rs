pub(crate) const BUDGET_WARNING_STATUS: &str = "rlm_context_budget_warning";

#[cfg(test)]
mod tests {
    use crate::rlm_support::{effective_budget_tokens, format_budget_suffix_with_vocabulary};
    use lash_core::TokenUsage;

    fn prompt_usage(used_tokens: usize) -> TokenUsage {
        TokenUsage {
            input_tokens: used_tokens as i64,
            ..TokenUsage::default()
        }
    }

    #[test]
    fn disabled_decomposition_finishes_at_budget_thresholds() {
        for vocabulary in [
            crate::dialect::DialectPromptVocabulary::default(),
            crate::dialect::typescript::TYPESCRIPT_PROMPT_VOCABULARY,
        ] {
            for used in [60, 90, 100, 110] {
                let usage = TokenUsage {
                    input_tokens: used as i64,
                    ..TokenUsage::default()
                };
                let text = format_budget_suffix_with_vocabulary(
                    2,
                    Some(&usage),
                    Some(100),
                    vocabulary,
                    false,
                )
                .unwrap();
                assert!(text.contains("finish concisely"), "{text}");
                assert!(!text.contains("continue_as"), "{text}");
                assert!(!text.contains("frame switch"), "{text}");
            }
        }
    }

    #[test]
    fn effective_frame_switch_threshold_stays_below_representative_context_windows() {
        for (configured_threshold, context_window_tokens, expected_threshold) in [
            (100_000, 41_000, 40_999),
            (30_000, 41_000, 30_000),
            (100_000, 200_000, 100_000),
        ] {
            let threshold =
                effective_budget_tokens(Some(configured_threshold), Some(context_window_tokens))
                    .expect("configured threshold should remain enabled");
            assert_eq!(threshold, expected_threshold);
            assert!(threshold < context_window_tokens);

            let content = format_budget_suffix_with_vocabulary(
                0,
                Some(&prompt_usage(threshold)),
                Some(threshold),
                crate::dialect::DialectPromptVocabulary::default(),
                true,
            )
            .expect("budget suffix should render");
            assert!(content.contains(&format!("frame switch threshold: {threshold}")));
        }
    }

    #[test]
    fn budget_prompt_contribution_below_advisory_floor_emits_status_only() {
        let usage = prompt_usage(47_213);
        let content = format_budget_suffix_with_vocabulary(
            0,
            Some(&usage),
            Some(200_000),
            crate::dialect::DialectPromptVocabulary::default(),
            true,
        )
        .expect("budget suffix should render");

        assert!(content.contains("Tokens: 47213 · frame switch threshold: 200000 (23%)"));
        assert!(content.contains("Turn:"));
        assert!(!content.contains("Look for a clean frame switch point"));
        assert!(!content.contains("Budget tight"));
        assert!(!content.contains("Past the frame switch threshold"));
    }

    #[test]
    fn budget_prompt_contribution_advisory_tier_60_to_89_pct() {
        let usage = prompt_usage(75_000);
        let content = format_budget_suffix_with_vocabulary(
            0,
            Some(&usage),
            Some(100_000),
            crate::dialect::DialectPromptVocabulary::default(),
            true,
        )
        .expect("budget suffix should render");

        assert!(content.contains("Tokens: 75000 · frame switch threshold: 100000 (75%)"));
        assert!(content.contains("Look for a clean frame switch point"));
        assert!(!content.contains("Budget tight"));
        assert!(!content.contains("Past the frame switch threshold"));
    }

    #[test]
    fn budget_prompt_contribution_tight_tier_90_to_99_pct() {
        let usage = prompt_usage(95_000);
        let content = format_budget_suffix_with_vocabulary(
            0,
            Some(&usage),
            Some(100_000),
            crate::dialect::DialectPromptVocabulary::default(),
            true,
        )
        .expect("budget suffix should render");

        assert!(content.contains("Tokens: 95000 · frame switch threshold: 100000 (95%)"));
        assert!(content.contains("Budget tight"));
        assert!(content.contains("`control.continue_as(...)`"));
        assert!(!content.contains("Past the frame switch threshold"));
        assert!(!content.contains("Look for a clean frame switch point"));
    }

    #[test]
    fn budget_prompt_contribution_over_threshold_forces_frame_switch() {
        let usage = prompt_usage(120_292);
        let content = format_budget_suffix_with_vocabulary(
            0,
            Some(&usage),
            Some(100_000),
            crate::dialect::DialectPromptVocabulary::default(),
            true,
        )
        .expect("budget suffix should render");

        assert!(content.contains("Tokens: 120292 · frame switch threshold: 100000 (120%)"));
        assert!(content.contains("Past the frame switch threshold"));
        assert!(content.contains("End this cell with `control.continue_as(...)` now"));
        assert!(content.contains("do not call `finish`"));
        assert!(content.contains("`task` + `seed`"));
    }

    #[test]
    fn budget_prompt_contribution_omits_without_configured_budget() {
        let usage = prompt_usage(47_213);

        assert!(
            format_budget_suffix_with_vocabulary(
                0,
                Some(&usage),
                None,
                crate::dialect::DialectPromptVocabulary::default(),
                true,
            )
            .is_none()
        );
    }

    #[test]
    fn budget_prompt_contribution_omits_without_used_tokens() {
        let usage = prompt_usage(0);

        assert!(
            format_budget_suffix_with_vocabulary(
                0,
                Some(&usage),
                Some(200_000),
                crate::dialect::DialectPromptVocabulary::default(),
                true,
            )
            .is_none()
        );
    }
}
