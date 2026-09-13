use lash_sqlite_store::{SqliteSessionStoreFactory, Store, StoreOptions};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::{Layer, layer::SubscriberExt};

#[derive(Clone, Default)]
struct Warnings(Arc<Mutex<Vec<BTreeMap<String, String>>>>);
impl<S: tracing::Subscriber> Layer<S> for Warnings {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        #[derive(Default)]
        struct Fields(BTreeMap<String, String>);
        impl tracing::field::Visit for Fields {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

lash_conformance::attachment_owner_degraded_tests!({
    let dir = tempfile::tempdir().unwrap();
    let factory = Arc::new(SqliteSessionStoreFactory::new(dir.path()))
        as Arc<dyn lash_core::SessionStoreFactory>;
    (dir, factory)
});

#[tokio::test]
async fn attachment_constructors_warn_exactly_once_with_fields() {
    for path in [
        "Store::open",
        "Store::open_with_clock",
        "Store::open_with_options",
        "Store::open_with_options_and_clock",
        "Store::memory",
        "Store::memory_with_clock",
        "Store::memory_with_options",
        "Store::memory_with_options_and_clock",
        "SqliteSessionStoreFactory::new",
        "SqliteSessionStoreFactory::with_options",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("store.db");
        let warnings = Warnings::default();
        let subscriber = tracing_subscriber::registry().with(warnings.clone());
        async {
            let clock = Arc::new(lash_core::facade_support::SystemClock);
            match path {
                "Store::open" => {
                    Store::open(&db).await.unwrap();
                }
                "Store::open_with_clock" => {
                    Store::open_with_clock(&db, clock).await.unwrap();
                }
                "Store::open_with_options" => {
                    Store::open_with_options(&db, StoreOptions::default())
                        .await
                        .unwrap();
                }
                "Store::open_with_options_and_clock" => {
                    Store::open_with_options_and_clock(&db, StoreOptions::default(), clock)
                        .await
                        .unwrap();
                }
                "Store::memory" => {
                    Store::memory().await.unwrap();
                }
                "Store::memory_with_clock" => {
                    Store::memory_with_clock(clock).await.unwrap();
                }
                "Store::memory_with_options" => {
                    Store::memory_with_options(StoreOptions::default())
                        .await
                        .unwrap();
                }
                "Store::memory_with_options_and_clock" => {
                    Store::memory_with_options_and_clock(StoreOptions::default(), clock)
                        .await
                        .unwrap();
                }
                _ => {
                    let factory = if path == "SqliteSessionStoreFactory::new" {
                        SqliteSessionStoreFactory::new(dir.path())
                    } else {
                        SqliteSessionStoreFactory::with_options(dir.path(), StoreOptions::default())
                    };
                    drop(factory);
                }
            }
        }
        .with_subscriber(subscriber)
        .await;
        let events = warnings.0.lock().unwrap();
        assert_eq!(events.len(), 1, "{path}: {events:?}");
        assert_eq!(events[0]["store"], "sqlite");
        assert_eq!(events[0]["path"], path);
        assert_eq!(
            events[0]["consequence"],
            "process-owned uncommitted intents are never reclaimed"
        );
    }
}
