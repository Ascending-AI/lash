pub(crate) const BUDGET_WARNING_STATUS: &str = "rlm_context_budget_warning";

#[cfg(test)]
mod tests {
    use crate::rlm_support::{effective_budget_tokens, format_budget_suffix_with_vocabulary};
    use lash_core::LlmUsage;

    fn prompt_usage(used_tokens: usize) -> LlmUsage {
        LlmUsage {
            input_tokens: used_tokens as i64,
            ..LlmUsage::default()
        }
    }

    #[test]
    fn disabled_decomposition_finishes_at_budget_thresholds() {
        {
            let vocabulary =
                crate::dialect::Dialect::prompt_vocabulary(&crate::dialect::TypescriptDialect);
            for used in [60, 90, 100, 110] {
                let usage = LlmUsage {
                    input_tokens: used as i64,
                    ..LlmUsage::default()
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
                crate::dialect::Dialect::prompt_vocabulary(&crate::dialect::TypescriptDialect),
                true,
            )
            .expect("budget suffix should render");
            assert!(content.contains(&format!("frame switch threshold: {threshold}")));
        }
    }

    #[test]
    fn budget_prompt_contribution_omits_without_configured_budget() {
        let usage = prompt_usage(47_213);

        assert!(
            format_budget_suffix_with_vocabulary(
                0,
                Some(&usage),
                None,
                crate::dialect::Dialect::prompt_vocabulary(&crate::dialect::TypescriptDialect),
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
                crate::dialect::Dialect::prompt_vocabulary(&crate::dialect::TypescriptDialect),
                true,
            )
            .is_none()
        );
    }
}
