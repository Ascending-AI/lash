/// A declared park announcement is the runtime's to append, so a failed append
/// has to fail the call. Parking anyway would leave a durable wait whose
/// announcement never happened — exactly the split the declaration exists to
/// prevent.
#[tokio::test]
async fn failed_park_announcement_fails_the_call_instead_of_parking() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let context = pending_dispatch_context(
        PendingProbeMode::AnnouncingWithoutProcess,
        Arc::clone(&attempts),
        None,
        ToolRetryPolicy::Never,
    );
    let prepared = pending_prepared_call();
    let tool_context = tool_context_for_prepared(&context, &prepared);

    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &context,
        prepared,
        None,
        tool_context,
    )
    .await;

    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("an unappendable announcement must not park the call");
    };
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let ToolCallOutcome::Failure(failure) = &outcome.record.output.outcome else {
        panic!("expected failure output");
    };
    assert_eq!(failure.code, "pending_tool_announcement_failed");
    assert!(
        failure.message.contains("announcement"),
        "the failure must say the declared announcement could not be appended: {}",
        failure.message
    );
}
