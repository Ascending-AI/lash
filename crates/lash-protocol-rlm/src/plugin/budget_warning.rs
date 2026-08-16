use lash_core::facade_support::PreparedContext;
use lash_core::plugin::{ContextError, TurnContextTransform, TurnTransformContext};
use lash_sansio::sync::RwLockExt;

use crate::driver::SharedPromptUsage;

pub(super) const BUDGET_WARNING_STATUS: &str = "rlm_context_budget_warning";

pub(super) struct BudgetUsageObserver {
    pub(super) cell: SharedPromptUsage,
}

#[async_trait::async_trait]
impl TurnContextTransform for BudgetUsageObserver {
    fn id(&self) -> &'static str {
        "rlm.budget_usage_observer"
    }

    async fn transform(
        &self,
        ctx: &TurnTransformContext<'_>,
        input: PreparedContext,
    ) -> Result<PreparedContext, ContextError> {
        *self.cell.write_recover() = ctx.prompt_usage.clone();
        Ok(input)
    }
}

#[cfg(test)]
mod tests {
    use crate::rlm_support::format_budget_suffix_with_vocabulary;

    fn prompt_usage(context_budget_tokens: usize) -> lash_core::PromptUsage {
        lash_core::PromptUsage {
            prompt_context_tokens: context_budget_tokens,
            input_tokens: context_budget_tokens,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            context_budget_tokens,
        }
    }

    #[test]
    fn budget_prompt_contribution_below_advisory_floor_emits_status_only() {
        let usage = prompt_usage(47_213);
        let content = format_budget_suffix_with_vocabulary(
            0,
            Some(&usage),
            Some(200_000),
            crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
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
            crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
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
            crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
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
            crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        )
        .expect("budget suffix should render");

        assert!(content.contains("Tokens: 120292 · frame switch threshold: 100000 (120%)"));
        assert!(content.contains("Past the frame switch threshold"));
        assert!(content.contains("End this block with `control.continue_as(...)` now"));
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
                crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY
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
                crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY
            )
            .is_none()
        );
    }
}
