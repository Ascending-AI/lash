use super::*;

impl From<lash_core::ConfigCommandEntry> for RemoteConfigCommandEntry {
    fn from(value: lash_core::ConfigCommandEntry) -> Self {
        let lash_core::ConfigCommandEntry {
            owner,
            command,
            args,
        } = value;
        Self {
            owner,
            command,
            args,
        }
    }
}

impl From<RemoteConfigCommandEntry> for lash_core::ConfigCommandEntry {
    fn from(value: RemoteConfigCommandEntry) -> Self {
        let RemoteConfigCommandEntry {
            owner,
            command,
            args,
        } = value;
        Self {
            owner,
            command,
            args,
        }
    }
}

impl RemoteConfigTransactionRequest {
    /// The transaction this request carries, its entries in order. Each
    /// entry's owner decodes its arguments when the session admits it.
    pub fn into_transaction(self) -> lash_core::ConfigTransaction {
        let Self {
            session_id: _,
            id: _,
            expected_revision: _,
            entries,
        } = self;
        entries
            .into_iter()
            .fold(lash_core::ConfigTransaction::new(), |transaction, entry| {
                transaction.then_entry(entry.into())
            })
    }
}

impl From<lash_core::ConfigRefusal> for RemoteConfigRefusal {
    fn from(value: lash_core::ConfigRefusal) -> Self {
        let lash_core::ConfigRefusal {
            index,
            owner,
            command,
            refusal,
            message,
        } = value;
        Self {
            index,
            owner,
            command,
            refusal,
            message,
        }
    }
}

impl From<RemoteConfigRefusal> for lash_core::ConfigRefusal {
    fn from(value: RemoteConfigRefusal) -> Self {
        let RemoteConfigRefusal {
            index,
            owner,
            command,
            refusal,
            message,
        } = value;
        Self {
            index,
            owner,
            command,
            refusal,
            message,
        }
    }
}

impl From<lash_core::ConfigTransactionOutcome> for RemoteConfigTransactionOutcome {
    fn from(value: lash_core::ConfigTransactionOutcome) -> Self {
        match value {
            lash_core::ConfigTransactionOutcome::Applied {
                base_revision,
                revision,
                outputs,
            } => Self::Applied {
                base_revision,
                revision,
                outputs,
            },
            lash_core::ConfigTransactionOutcome::Stale { expected, actual } => {
                Self::Stale { expected, actual }
            }
            lash_core::ConfigTransactionOutcome::Refused { refusal } => Self::Refused {
                refusal: refusal.into(),
            },
        }
    }
}

impl From<RemoteConfigTransactionOutcome> for lash_core::ConfigTransactionOutcome {
    fn from(value: RemoteConfigTransactionOutcome) -> Self {
        match value {
            RemoteConfigTransactionOutcome::Applied {
                base_revision,
                revision,
                outputs,
            } => Self::Applied {
                base_revision,
                revision,
                outputs,
            },
            RemoteConfigTransactionOutcome::Stale { expected, actual } => {
                Self::Stale { expected, actual }
            }
            RemoteConfigTransactionOutcome::Refused { refusal } => Self::Refused {
                refusal: refusal.into(),
            },
        }
    }
}

impl From<lash_core::ConfigCommandDescriptor> for RemoteConfigCommandDescriptor {
    fn from(value: lash_core::ConfigCommandDescriptor) -> Self {
        let lash_core::ConfigCommandDescriptor {
            owner,
            command,
            input_schema,
            output_schema,
            refusal_schema,
        } = value;
        Self {
            owner,
            command,
            input_schema,
            output_schema,
            refusal_schema,
        }
    }
}

impl From<lash_core::ConfigCommandCatalog> for RemoteConfigCommandCatalog {
    fn from(value: lash_core::ConfigCommandCatalog) -> Self {
        let lash_core::ConfigCommandCatalog { revision, commands } = value;
        Self {
            revision,
            commands: commands.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal() -> lash_core::ConfigRefusal {
        lash_core::ConfigRefusal {
            index: Some(1),
            owner: "probe".to_string(),
            command: Some("raise_cap".to_string()),
            refusal: serde_json::json!({ "kind": "cap_lowered", "recorded": 8, "requested": 4 }),
            message: "the cap may only rise".to_string(),
        }
    }

    #[test]
    fn every_config_outcome_survives_the_wire_and_the_core_round_trip() {
        for outcome in [
            lash_core::ConfigTransactionOutcome::Applied {
                base_revision: 3,
                revision: 4,
                outputs: vec![serde_json::json!(8), serde_json::Value::Null],
            },
            lash_core::ConfigTransactionOutcome::Stale {
                expected: 3,
                actual: 5,
            },
            lash_core::ConfigTransactionOutcome::Refused { refusal: refusal() },
        ] {
            let remote = RemoteConfigTransactionOutcome::from(outcome.clone());
            let wire = serde_json::to_value(&remote).expect("outcome encodes");
            let decoded: RemoteConfigTransactionOutcome =
                serde_json::from_value(wire).expect("outcome decodes");
            assert_eq!(decoded, remote);
            assert_eq!(lash_core::ConfigTransactionOutcome::from(decoded), outcome);
        }
    }

    #[test]
    fn a_whole_candidate_refusal_carries_no_index_or_command() {
        let refusal = lash_core::ConfigRefusal {
            index: None,
            command: None,
            ..refusal()
        };
        let wire = serde_json::to_value(RemoteConfigRefusal::from(refusal.clone()))
            .expect("refusal encodes");
        assert_eq!(wire.get("index"), None);
        assert_eq!(wire.get("command"), None);
        let decoded: RemoteConfigRefusal = serde_json::from_value(wire).expect("refusal decodes");
        assert_eq!(lash_core::ConfigRefusal::from(decoded), refusal);
    }

    #[test]
    fn a_request_decodes_strictly_and_validates_its_entries() {
        let wire = serde_json::json!({
            "session_id": "remote-config",
            "id": "tx-1",
            "expected_revision": 2,
            "entries": [
                { "owner": "core", "command": "set_turn_budget", "args": { "turn_budget": "unbounded" } },
            ],
        });
        let request: RemoteConfigTransactionRequest =
            serde_json::from_value(wire.clone()).expect("request decodes");
        request.validate().expect("a complete request validates");
        assert_eq!(
            serde_json::to_value(&request).expect("request encodes"),
            wire
        );

        let mut unknown = wire.clone();
        unknown["replacements"] = serde_json::json!({});
        assert!(
            serde_json::from_value::<RemoteConfigTransactionRequest>(unknown).is_err(),
            "a request carries commands only, never a replacement"
        );

        let empty = RemoteConfigTransactionRequest {
            entries: Vec::new(),
            ..request.clone()
        };
        assert!(empty.validate().is_err(), "an empty transaction is refused");
        let mut unnamed = request;
        unnamed.entries[0].command.clear();
        assert!(unnamed.validate().is_err(), "an unnamed command is refused");
    }

    #[test]
    fn the_catalog_keeps_every_descriptor_and_its_revision() {
        let catalog = lash_core::ConfigCommandCatalog {
            revision: 7,
            commands: vec![lash_core::ConfigCommandDescriptor {
                owner: "core".to_string(),
                command: "set_turn_budget".to_string(),
                input_schema: serde_json::json!({ "type": "object" }),
                output_schema: serde_json::json!({ "type": "null" }),
                refusal_schema: serde_json::json!({ "oneOf": [] }),
            }],
        };
        let remote = RemoteConfigCommandCatalog::from(catalog.clone());
        assert_eq!(remote.revision, 7);
        assert_eq!(remote.commands.len(), 1);
        assert_eq!(remote.commands[0].command, "set_turn_budget");
        assert_eq!(
            remote.commands[0].input_schema,
            catalog.commands[0].input_schema
        );
        let decoded: RemoteConfigCommandCatalog =
            serde_json::from_value(serde_json::to_value(&remote).expect("catalog encodes"))
                .expect("catalog decodes");
        assert_eq!(decoded, remote);
    }
}
