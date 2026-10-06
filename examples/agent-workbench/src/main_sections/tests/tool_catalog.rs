use super::*;

pub(crate) fn catalog_lifecycle_provider() -> lash::provider::ProviderHandle {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    lash::testing::TestProvider::builder()
        .kind("workbench-test")
        .complete(move |_| {
            let calls = Arc::clone(&calls);
            async move {
                let account = match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => "test",
                    1 => "live",
                    call => panic!("unexpected workbench provider call {call}"),
                };
                Ok(text_response(&format!(
                    "<typescript>\nconst result = await inbox.{account}.send({{ title: \"Hi\", text: \"Yo\" }});\nfinish(result.id);\n</typescript>"
                )))
            }
        })
        .build()
        .into_handle()
}

pub(crate) async fn assert_tool_catalog_contract(session: &lash::LashSession) {
    // A catalog is a session's: read from the session's recorded facts, the
    // manifests its durable head recorded (FIG-5139).
    let session_tools = session.admin().tools();
    let active_send = || async {
        session_tools
            .active_manifests()
            .await
            .expect("read active session manifests")
            .into_iter()
            .find(|manifest| manifest.name == "inbox__test__send")
    };
    let send_manifest = active_send()
        .await
        .expect("the session catalog composes the workbench plugin's inbox tool");
    let compact = send_manifest
        .compact_contract
        .as_ref()
        .expect("the recorded manifest carries its compact contract");
    assert!(
        compact.signature.contains("title"),
        "the recorded contract names the runtime input schema: {}",
        compact.signature
    );

    session_tools
        .set_membership(send_manifest.id.clone(), false)
        .await
        .expect("remove send from this session catalog");
    assert!(
        active_send().await.is_none(),
        "a non-member tool leaves the session's recorded catalog"
    );
    session_tools
        .set_membership(send_manifest.id, true)
        .await
        .expect("restore send to this session catalog");
    assert!(
        active_send().await.is_some(),
        "membership restores the tool to the session's recorded catalog"
    );
}

pub(crate) async fn assert_plugin_provider_execution(
    session: &lash::LashSession,
    plugin_mail_world: &mail::MailWorld,
    account: &str,
) {
    let output = session
        .send(lash::TurnInput::text("send through the plugin provider"))
        .id(lash::TurnId::fixture(format!(
            "workbench-test-turn:{}",
            uuid::Uuid::new_v4()
        )))
        .output()
        .await
        .expect("turn executes the account through its installed plugin revision");
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!(format!("{account}-1")))
    );
    assert_eq!(
        plugin_mail_world
            .inbox(account)
            .expect("installed account inbox")
            .len(),
        1
    );
}
