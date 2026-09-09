//! The effect journal the SQLite process registry attaches so a
//! registration's fence release shares the registry insert's transaction
//! (FIG-2499, ADR 0049).

use super::*;

/// The journal key of a process scope, as the fence table stores it.
pub(super) fn process_scope_fence_key(process_id: &str) -> Result<String, lash_core::PluginError> {
    lash_core::ExecutionScope::process(process_id)
        .journal_identity()
        .map(|identity| identity.key().to_string())
        .map_err(|error| lash_core::PluginError::Session(error.to_string()))
}

/// Schema name under which a bound host's journal file is attached.
const EFFECT_JOURNAL_SCHEMA: &str = "effect_journal";

/// The bound effect host's journal file, attached to the registry connection
/// on first use so registration can clear the scope fence in its own write.
///
/// `ATTACH` cannot run inside a transaction, so the attach is a separate
/// serialized step ahead of the registration write; once attached it stays
/// for the connection's lifetime. A journal that turns out to be the registry
/// file itself is addressed through `main` instead.
#[derive(Default)]
pub(crate) struct EffectJournalAttachment {
    state: std::sync::Mutex<EffectJournalAttachmentState>,
    attach: tokio::sync::Mutex<()>,
}

#[derive(Default)]
enum EffectJournalAttachmentState {
    #[default]
    Unbound,
    Requested(PathBuf),
    Attached {
        path: PathBuf,
        schema: &'static str,
    },
}

impl EffectJournalAttachment {
    pub(super) fn request(&self, path: PathBuf) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &*state {
            EffectJournalAttachmentState::Attached { path: attached, .. } if *attached == path => {}
            EffectJournalAttachmentState::Attached { path: attached, .. } => {
                tracing::warn!(
                    attached = %attached.display(),
                    requested = %path.display(),
                    "process registry already attached a different effect journal; the \
                     later host's fence is lifted after the registration write instead"
                );
            }
            _ => *state = EffectJournalAttachmentState::Requested(path),
        }
    }

    /// Attach the requested journal if not yet attached; returns the schema
    /// name to address its fence table through, or `None` when no journal
    /// file is bound.
    pub(super) async fn ensure_attached(
        &self,
        conn: &SqliteConnection,
    ) -> Result<Option<&'static str>, lash_core::PluginError> {
        let _serialized = self.attach.lock().await;
        let requested = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match &*state {
                EffectJournalAttachmentState::Unbound => return Ok(None),
                EffectJournalAttachmentState::Attached { schema, .. } => return Ok(Some(schema)),
                EffectJournalAttachmentState::Requested(path) => path.clone(),
            }
        };
        let path = requested.clone();
        let schema = conn
            .call(move |connection| {
                let main_file: Option<String> = connection
                    .query_row(
                        "SELECT file FROM pragma_database_list WHERE name = 'main'",
                        [],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();
                if main_file
                    .as_deref()
                    .is_some_and(|main| !main.is_empty() && Path::new(main) == path)
                {
                    return Ok("main");
                }
                connection.execute(
                    &format!("ATTACH DATABASE ?1 AS {EFFECT_JOURNAL_SCHEMA}"),
                    params![path.to_string_lossy().into_owned()],
                )?;
                Ok(EFFECT_JOURNAL_SCHEMA)
            })
            .await
            .map_err(process_sqlite_error)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *state = EffectJournalAttachmentState::Attached {
            path: requested,
            schema,
        };
        Ok(Some(schema))
    }
}
