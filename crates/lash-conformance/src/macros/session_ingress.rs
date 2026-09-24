//! Registration macro for the session-ingress store laws (ADR 0101 §16).

/// Register one independently reported test per session-ingress store law.
///
/// The fixture block yields `(guard, handles)`: a value kept alive for the
/// test's duration, and the [`SessionIngressHandles`](crate::SessionIngressHandles)
/// of a fresh session named
/// [`SESSION_INGRESS_SESSION_ID`](crate::SESSION_INGRESS_SESSION_ID).
#[macro_export]
macro_rules! session_ingress_tests {
    ($fixture:block) => {
        $crate::session_ingress_tests!(@catalogue $fixture; [
            (ingress_order_is_one_sequence_across_kinds, "ingress-order"),
            (idle_claim_takes_the_fifo_prefix, "ingress-fifo-prefix"),
            (turn_claim_stops_and_never_skips, "ingress-stop-points"),
            (command_lane_never_blocks_or_waits_for_the_turn_lane, "ingress-command-lane"),
            (turn_addressed_items_follow_their_turn, "ingress-addressed-items"),
            (adjacent_config_patches_share_one_command_claim, "ingress-coalescing"),
            (replay_compares_the_immutable_digest_for_every_kind, "ingress-dedup"),
            (reserved_source_key_prefixes_are_refused, "ingress-reserved-prefixes"),
            (tombstones_stay_until_vacuum_and_never_reopen, "ingress-tombstones"),
            (every_wake_terminal_raises_the_redelivery_floor, "ingress-wake-floor"),
            (a_turn_cancel_disposes_by_author, "ingress-cancel-by-author"),
            (a_deferred_claim_is_recomposed_row_by_row, "ingress-recompose"),
            (a_reclaimed_row_supersedes_the_old_claim, "ingress-fencing"),
            (a_suffix_withdrawal_stays_in_its_lane, "ingress-suffix-withdrawal"),
            (a_resumed_claim_reclaims_every_row_it_owns, "ingress-own-row-reclaim"),
            (a_redrive_superseded_by_a_peer_cedes, "ingress-peer-supersession-cedes"),
            (the_drive_epoch_seal_is_idempotent_per_admission, "ingress-drive-epoch-seal"),
            (a_turn_cancel_reaches_rows_an_interrupted_claim_holds, "ingress-cancel-interrupted-hold"),
            (concurrent_seals_serialize, "ingress-concurrent-seals"),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_fixture_guard, handles) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(handles).await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}
