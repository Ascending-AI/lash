//! Structural refusals at the SDK's live Call ingress, before any handler write.

use std::sync::Arc;

use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
use lash_sqlite_store::{SqliteDatabase, SqliteStoreSet};

#[expect(
    clippy::disallowed_methods,
    reason = "live law reads its service gate's injected environment"
)]
fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required by the live JSON decode law"))
}

fn config(endpoint: &str) -> LiveConfig {
    let tag = format!("json4293{}", uuid::Uuid::new_v4().simple());
    LiveConfig {
        ingress_url: env("RESTATE_INGRESS_URL"),
        admin_url: env("RESTATE_ADMIN_URL"),
        endpoint_bind: env(&format!("{endpoint}_BIND"))
            .parse()
            .expect("endpoint bind"),
        endpoint_url: env(&format!("{endpoint}_URL")),
        namespace: tag.parse().expect("namespace"),
        run_tag: tag,
    }
}

fn sqlite_observers(stores: &SqliteStoreSet) -> Vec<(rusqlite::Connection, i64)> {
    [
        SqliteDatabase::DurableCore,
        SqliteDatabase::ProcessRegistry,
        SqliteDatabase::Triggers,
    ]
    .into_iter()
    .map(|database| {
        let connection =
            rusqlite::Connection::open(stores.database_uri(database)).expect("open SQL observer");
        let version = connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .expect("data version");
        (connection, version)
    })
    .collect()
}

fn assert_sqlite_unchanged(observers: &[(rusqlite::Connection, i64)]) {
    for (connection, before) in observers {
        let after: i64 = connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .expect("data version");
        assert_eq!(*before, after, "refusal committed no SQLite writes");
    }
}

async fn ingress_law(backend: &LiveRestateBackend<dyn lash_core::StoreSet>) {
    let service = format!("{}.EffectGroupPayload", backend.namespace());
    let ingress = backend.ingress();
    let wide = crate::Call::new(crate::EffectGroupPayloadPutRequest {
        bytes: vec![0; crate::JsonDecodeLimits::default().max_nodes + 1],
    });
    let encoded = serde_json::to_vec(&wide).unwrap();
    assert!(encoded.len() < crate::JsonDecodeLimits::default().max_bytes);
    let error = ingress
        .call_object_json::<_, crate::Reply<crate::EffectGroupPayloadPutResponse>>(
            &service, "wide", "put", &wide,
        )
        .await
        .expect_err("wide ingress refuses before put");
    assert!(
        error.to_string().contains("JSON decode nodes limit"),
        "{error}"
    );

    let peer = crate::RESTATE_WIRE.max() + 1;
    let unsupported = r#"{"body":{"bytes":[1e999]},"line":1,"wire":{"min":PEER,"max":PEER}}"#
        .replace("PEER", &peer.to_string());
    let response = reqwest::Client::new()
        .post(format!(
            "{}/{service}/unsupported/put",
            env("RESTATE_INGRESS_URL")
        ))
        .header("content-type", "application/json")
        .body(unsupported)
        .send()
        .await
        .expect("call unsupported wire");
    assert!(!response.status().is_success());
    let response = response.text().await.expect("read version refusal");
    assert!(response.contains("lash.wire_unsupported"), "{response}");

    let invocations = backend
        .invocations()
        .await
        .expect("read refused invocations");
    assert_eq!(
        invocations.len(),
        2,
        "both requests reached the native invoker"
    );
    for invocation in invocations {
        let journal = backend
            .journal(&invocation.id)
            .await
            .expect("read refusal journal");
        assert!(!journal.is_empty(), "the native invocation has a journal");
        for entry in &journal {
            let command = entry
                .split(':')
                .nth(1)
                .expect("journal command")
                .to_ascii_lowercase();
            assert!(
                !["state", "run", "call", "send"]
                    .iter()
                    .any(|name| command.contains(name)),
                "refusal ran no handler commands: {journal:?}"
            );
        }
    }

    let accepted = ingress
        .call_object_json::<_, crate::Reply<crate::EffectGroupPayloadPutResponse>>(
            &service,
            "control",
            "put",
            &crate::Call::new(crate::EffectGroupPayloadPutRequest { bytes: vec![7] }),
        )
        .await
        .expect("a bounded Call reaches put");
    assert_eq!(accepted.body, crate::EffectGroupPayloadPutResponse::Written);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "native Restate and PostgreSQL: json-decode Restate service suite"]
async fn live_call_decode_refuses_before_writes_on_sqlite_and_postgres() {
    let root = tempfile::tempdir().expect("file store root");
    let memory = SqliteStoreSet::memory().await.expect("memory stores");
    let file = SqliteStoreSet::open(root.path())
        .await
        .expect("file stores");
    for (endpoint, stores) in [("JSON_MEM", memory), ("JSON_FILE", file)] {
        let observer_stores = stores.clone();
        let backend =
            LiveRestateBackend::start_with_store_set(config(endpoint), |clock| async move {
                let stores = stores.reopen_with_clock(clock).await.map_err(|error| {
                    lash_restate_test::live::LiveError::Stores(error.to_string())
                })?;
                Ok(Arc::new(stores) as Arc<dyn lash_core::StoreSet>)
            })
            .await
            .expect("live SQLite endpoint");
        let observers = sqlite_observers(&observer_stores);
        ingress_law(&backend).await;
        assert_sqlite_unchanged(&observers);
        backend.stop_serving(true);
    }

    let database =
        lash_postgres_store::testing::IsolatedDatabase::create(&env("LASH_POSTGRES_DATABASE_URL"))
            .await;
    let storage = lash_postgres_store::PostgresStorage::connect(database.url())
        .await
        .expect("PostgreSQL stores");
    let attachments = tempfile::tempdir().expect("attachments");
    let backend = LiveRestateBackend::start_with_store_set(config("JSON_PG"), |clock| {
        let stores = lash_postgres_store::PostgresStoreSet::with_clock(
            &storage,
            Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                attachments.path(),
            )),
            lash_core::WakeDeliveryConfig::default(),
            clock,
        );
        async move { Ok(Arc::new(stores) as Arc<dyn lash_core::StoreSet>) }
    })
    .await
    .expect("live PostgreSQL endpoint");
    sqlx::raw_sql(
        "CREATE TABLE decode_writes (table_name text NOT NULL);
        CREATE FUNCTION record_decode_write() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN INSERT INTO decode_writes VALUES (TG_TABLE_NAME); RETURN NULL; END $$;",
    )
    .execute(storage.pool())
    .await
    .expect("install write observer");
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname = 'public' AND tablename LIKE 'lash\\_%'",
    )
    .fetch_all(storage.pool())
    .await
    .expect("list backend tables");
    assert!(!tables.is_empty());
    for table in tables {
        let table = table.replace('"', "\"\"");
        sqlx::raw_sql(&format!("CREATE TRIGGER decode_write AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE ON \"{table}\" FOR EACH STATEMENT EXECUTE FUNCTION record_decode_write()"))
            .execute(storage.pool()).await.expect("observe backend writes");
    }
    ingress_law(&backend).await;
    let writes: i64 = sqlx::query_scalar("SELECT count(*) FROM decode_writes")
        .fetch_one(storage.pool())
        .await
        .expect("read backend writes");
    assert_eq!(writes, 0, "refusals wrote no PostgreSQL backend tables");
    backend.stop_serving(true);
}
