use lash_sqlite_store::{SqliteStore, StoreOptions};
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
    let catalog = Arc::new(SqliteStore::open(dir.path()).await.unwrap())
        as Arc<dyn lash_core_execution::DeploymentStore>;
    let attachments = Arc::new(
        lash_core_execution::facade_support::FileAttachmentStore::new(
            dir.path().join("attachments"),
        ),
    ) as Arc<dyn lash_core_execution::AttachmentStore>;
    (dir, catalog, attachments)
});

lash_conformance::retained_output_reclamation_tests!({
    let dir = tempfile::tempdir().unwrap();
    let catalog = Arc::new(SqliteStore::open(dir.path()).await.unwrap())
        as Arc<dyn lash_core_execution::DeploymentStore>;
    let attachments = Arc::new(
        lash_core_execution::facade_support::FileAttachmentStore::new(
            dir.path().join("attachments"),
        ),
    ) as Arc<dyn lash_core_execution::AttachmentStore>;
    (dir, catalog, attachments)
});

#[tokio::test]
async fn attachment_constructors_warn_exactly_once_with_fields() {
    for path in [
        "SqliteStore::open",
        "SqliteStore::open_with_clock",
        "SqliteStore::open_with_options",
        "SqliteStore::open_with_options_and_clock",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let warnings = Warnings::default();
        let subscriber = tracing_subscriber::registry().with(warnings.clone());
        async {
            let clock = Arc::new(lash_core_execution::facade_support::SystemClock);
            match path {
                "SqliteStore::open" => {
                    SqliteStore::open(dir.path()).await.unwrap();
                }
                "SqliteStore::open_with_clock" => {
                    SqliteStore::open_with_clock(dir.path(), clock)
                        .await
                        .unwrap();
                }
                "SqliteStore::open_with_options" => {
                    SqliteStore::open_with_options(dir.path(), StoreOptions::default())
                        .await
                        .unwrap();
                }
                "SqliteStore::open_with_options_and_clock" => {
                    SqliteStore::open_with_options_and_clock(
                        dir.path(),
                        StoreOptions::default(),
                        clock,
                    )
                    .await
                    .unwrap();
                }
                _ => unreachable!("the constructor list is exhaustive"),
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
