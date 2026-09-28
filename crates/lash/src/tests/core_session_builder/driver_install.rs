use super::*;

/// Holds a provider's first request: it signals `entered` and answers only
/// once `open` is notified, so the drive that made it stays in flight.
#[derive(Clone, Default)]
struct HeldFirstRequest {
    entered: Arc<tokio::sync::Notify>,
    open: Arc<tokio::sync::Notify>,
}

/// A provider that records `tag` for every request it answers, holding the
/// first one on `held` when given.
fn tagged_provider(
    tag: &'static str,
    calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
    held: Option<HeldFirstRequest>,
) -> ProviderHandle {
    let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let calls = Arc::clone(&calls);
            let held = held
                .clone()
                .filter(|_| first.swap(false, std::sync::atomic::Ordering::SeqCst));
            async move {
                calls.lock_recover().push(tag);
                if let Some(held) = held {
                    held.entered.notify_one();
                    held.open.notified().await;
                }
                Ok(text_response(tag))
            }
        })
        .build()
        .into_handle()
}

/// A drive holds the driver it runs on until it ends, which may be after
/// the core that installed it is gone (FIG-3979). A core built over the same
/// backend meanwhile installs its own driver, so its sessions run on its own
/// provider, not the gone core's (FIG-4017).
#[tokio::test]
async fn a_core_built_while_a_dropped_cores_drive_is_in_flight_drives_on_its_own_provider()
-> Result<()> {
    let backend = double_backend().await;
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let held = HeldFirstRequest::default();
    let core_v1 = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(tagged_provider(
        "v1",
        Arc::clone(&calls),
        Some(held.clone()),
    ))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    // No session stays open on V1, so nothing but V1 itself and its
    // in-flight drive holds V1's driver.
    drop(core_v1.session("first-core-session").open().await?);
    let handle = core_v1
        .session("first-core-session")
        .durable()
        .await?
        .send(TurnInput::text("held on V1"))
        .await?;
    drop(handle);
    held.entered.notified().await;
    drop(core_v1);

    let core_v2 = explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .provider(tagged_provider("v2", Arc::clone(&calls), None))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core_v2.session("second-core-session").open().await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        session.send(TurnInput::text("run on V2")).output(),
    )
    .await
    .expect("V2's turn settles")?;

    assert_eq!(
        calls.lock_recover().clone(),
        ["v1", "v2"],
        "V2's session ran on V2's provider while V1's drive was still in flight"
    );
    held.open.notify_one();
    Ok(())
}
