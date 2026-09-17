use lash::SessionId;
use lash::TurnId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WorkbenchQueuedTurnWorkflowRequest {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub reason: String,
    #[serde(flatten)]
    pub scope: QueuedTurnScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_id: Option<String>,
}

/// Which queued work a drain run covers. `All` drains every pending batch;
/// `Selected` drains exactly the listed batch ids. The scope is explicit in the
/// durable payload rather than encoded as `batch_ids` emptiness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub(crate) enum QueuedTurnScope {
    All,
    Selected { batch_ids: Vec<String> },
}

impl<'de> Deserialize<'de> for WorkbenchQueuedTurnWorkflowRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Payloads journaled before the tagged scope existed carry a bare
        // `batch_ids` vector whose emptiness was the discriminator; decode that
        // shape into the same enum.
        #[derive(Deserialize)]
        struct Wire {
            turn_id: TurnId,
            session_id: SessionId,
            reason: String,
            #[serde(default)]
            drain_id: Option<String>,
            #[serde(flatten)]
            rest: serde_json::Map<String, serde_json::Value>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let scope = if wire.rest.contains_key("scope") {
            let legacy_ids = wire.rest.get("batch_ids").cloned();
            let scope: QueuedTurnScope =
                serde_json::from_value(serde_json::Value::Object(wire.rest))
                    .map_err(serde::de::Error::custom)?;
            match &scope {
                QueuedTurnScope::Selected { batch_ids } if batch_ids.is_empty() => {
                    return Err(serde::de::Error::custom(
                        "a `selected` scope requires at least one batch id",
                    ));
                }
                QueuedTurnScope::All if legacy_ids.is_some() => {
                    return Err(serde::de::Error::custom(
                        "an `all` scope cannot carry `batch_ids`",
                    ));
                }
                _ => scope,
            }
        } else {
            let batch_ids = match wire.rest.get("batch_ids") {
                Some(value) => serde_json::from_value::<Vec<String>>(value.clone())
                    .map_err(serde::de::Error::custom)?,
                None => Vec::new(),
            };
            if batch_ids.is_empty() {
                QueuedTurnScope::All
            } else {
                QueuedTurnScope::Selected { batch_ids }
            }
        };
        Ok(Self {
            turn_id: wire.turn_id,
            session_id: wire.session_id,
            reason: wire.reason,
            scope,
            drain_id: wire.drain_id,
        })
    }
}

impl WorkbenchQueuedTurnWorkflowRequest {
    /// The idempotency key a drain reports under: the explicit override when
    /// the caller pinned one, else the turn id.
    pub(crate) fn drain_id(&self) -> String {
        self.drain_id
            .clone()
            .unwrap_or_else(|| self.turn_id.clone().to_string())
    }

    /// Builder for `QueuedTurnScope::All`.
    pub(crate) fn queued_turn(&self, session: &lash::LashSession) -> lash::QueuedTurnBuilder {
        session.queued_turn().drain_id(self.drain_id())
    }

    /// Builder for `QueuedTurnScope::Selected`.
    pub(crate) fn selected_queued_turn(
        &self,
        session: &lash::LashSession,
        batch_ids: &[String],
    ) -> lash::SelectedQueuedTurnBuilder {
        session
            .queued_turn()
            .batch_ids(batch_ids.iter().cloned())
            .drain_id(self.drain_id())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn request_json(extra: serde_json::Value) -> serde_json::Value {
        let mut value = json!({
            "turn_id": "qt-1",
            "session_id": "chat-1",
            "reason": "test",
        });
        value
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        value
    }

    #[test]
    fn legacy_empty_batch_ids_decodes_as_all() {
        let request: WorkbenchQueuedTurnWorkflowRequest =
            serde_json::from_value(request_json(json!({ "batch_ids": [] })))
                .expect("legacy payload decodes");
        assert_eq!(request.scope, QueuedTurnScope::All);
    }

    #[test]
    fn legacy_batch_ids_decode_as_selected() {
        let request: WorkbenchQueuedTurnWorkflowRequest =
            serde_json::from_value(request_json(json!({ "batch_ids": ["b-1", "b-2"] })))
                .expect("legacy payload decodes");
        assert_eq!(
            request.scope,
            QueuedTurnScope::Selected {
                batch_ids: vec!["b-1".to_string(), "b-2".to_string()],
            }
        );
    }

    #[test]
    fn tagged_scope_round_trips() {
        let request = WorkbenchQueuedTurnWorkflowRequest {
            turn_id: TurnId::from("qt-1"),
            session_id: SessionId::from("chat-1"),
            reason: "test".to_string(),
            scope: QueuedTurnScope::Selected {
                batch_ids: vec!["b-1".to_string()],
            },
            drain_id: None,
        };
        let decoded: WorkbenchQueuedTurnWorkflowRequest =
            serde_json::from_value(serde_json::to_value(&request).expect("serialize"))
                .expect("tagged payload decodes");
        assert_eq!(decoded.scope, request.scope);
    }

    #[test]
    fn an_all_scope_carrying_batch_ids_is_rejected() {
        assert!(
            serde_json::from_value::<WorkbenchQueuedTurnWorkflowRequest>(request_json(json!({
                "scope": "all",
                "batch_ids": ["b-1"],
            })))
            .is_err(),
            "a contradictory payload must not silently drop the selection"
        );
    }

    #[test]
    fn tagged_selected_with_no_batches_is_rejected() {
        assert!(
            serde_json::from_value::<WorkbenchQueuedTurnWorkflowRequest>(request_json(json!({
                "scope": "selected",
                "batch_ids": [],
            })))
            .is_err(),
            "an explicit selected scope must name at least one batch"
        );
    }
}
