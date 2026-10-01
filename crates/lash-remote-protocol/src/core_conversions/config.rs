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

impl From<lash_core::RefusalSite> for RemoteRefusalSite {
    fn from(value: lash_core::RefusalSite) -> Self {
        match value {
            lash_core::RefusalSite::Command { index, command } => Self::Command { index, command },
            lash_core::RefusalSite::Candidate => Self::Candidate,
            lash_core::RefusalSite::Creation => Self::Creation,
        }
    }
}

impl From<RemoteRefusalSite> for lash_core::RefusalSite {
    fn from(value: RemoteRefusalSite) -> Self {
        match value {
            RemoteRefusalSite::Command { index, command } => Self::Command { index, command },
            RemoteRefusalSite::Candidate => Self::Candidate,
            RemoteRefusalSite::Creation => Self::Creation,
        }
    }
}

impl From<lash_core::ConfigValueRole> for RemoteConfigValueRole {
    fn from(value: lash_core::ConfigValueRole) -> Self {
        match value {
            lash_core::ConfigValueRole::CreationInput => Self::CreationInput,
            lash_core::ConfigValueRole::Arguments => Self::Arguments,
            lash_core::ConfigValueRole::RunOptions => Self::RunOptions,
            lash_core::ConfigValueRole::Candidate => Self::Candidate,
            lash_core::ConfigValueRole::Output => Self::Output,
            lash_core::ConfigValueRole::Refusal => Self::Refusal,
        }
    }
}

impl From<RemoteConfigValueRole> for lash_core::ConfigValueRole {
    fn from(value: RemoteConfigValueRole) -> Self {
        match value {
            RemoteConfigValueRole::CreationInput => Self::CreationInput,
            RemoteConfigValueRole::Arguments => Self::Arguments,
            RemoteConfigValueRole::RunOptions => Self::RunOptions,
            RemoteConfigValueRole::Candidate => Self::Candidate,
            RemoteConfigValueRole::Output => Self::Output,
            RemoteConfigValueRole::Refusal => Self::Refusal,
        }
    }
}

impl From<lash_core::ConfigRefusalReason> for RemoteConfigRefusalReason {
    fn from(value: lash_core::ConfigRefusalReason) -> Self {
        match value {
            lash_core::ConfigRefusalReason::Owner { refusal, message } => {
                Self::Owner { refusal, message }
            }
            lash_core::ConfigRefusalReason::UnknownOwner => Self::UnknownOwner,
            lash_core::ConfigRefusalReason::UnknownCommand => Self::UnknownCommand,
            lash_core::ConfigRefusalReason::UnrecordedNamespace => Self::UnrecordedNamespace,
            lash_core::ConfigRefusalReason::Unreadable { role, message } => Self::Unreadable {
                role: role.into(),
                message,
            },
        }
    }
}

impl From<RemoteConfigRefusalReason> for lash_core::ConfigRefusalReason {
    fn from(value: RemoteConfigRefusalReason) -> Self {
        match value {
            RemoteConfigRefusalReason::Owner { refusal, message } => {
                Self::Owner { refusal, message }
            }
            RemoteConfigRefusalReason::UnknownOwner => Self::UnknownOwner,
            RemoteConfigRefusalReason::UnknownCommand => Self::UnknownCommand,
            RemoteConfigRefusalReason::UnrecordedNamespace => Self::UnrecordedNamespace,
            RemoteConfigRefusalReason::Unreadable { role, message } => Self::Unreadable {
                role: role.into(),
                message,
            },
        }
    }
}

impl From<lash_core::ConfigRefusal> for RemoteConfigRefusal {
    fn from(value: lash_core::ConfigRefusal) -> Self {
        let lash_core::ConfigRefusal { owner, at, reason } = value;
        Self {
            owner,
            at: at.into(),
            reason: reason.into(),
        }
    }
}

impl From<RemoteConfigRefusal> for lash_core::ConfigRefusal {
    fn from(value: RemoteConfigRefusal) -> Self {
        let RemoteConfigRefusal { owner, at, reason } = value;
        Self {
            owner,
            at: at.into(),
            reason: reason.into(),
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
            owner: "probe".to_string(),
            at: lash_core::RefusalSite::Command {
                index: 1,
                command: "raise_cap".to_string(),
            },
            reason: lash_core::ConfigRefusalReason::Owner {
                refusal: serde_json::json!({ "kind": "cap_lowered", "recorded": 8, "requested": 4 }),
                message: "the cap may only rise".to_string(),
            },
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

    /// Every site and every reason is its own tagged variant on the wire,
    /// and only the owner's reason carries the owner's data (FIG-4652).
    #[test]
    fn every_refusal_site_and_reason_is_its_own_tagged_variant() {
        use lash_core::{ConfigRefusalReason as Reason, ConfigValueRole, RefusalSite};
        let command = || RefusalSite::Command {
            index: 0,
            command: "raise_cap".to_string(),
        };
        for (at, reason, site_kind, reason_kind) in [
            (command(), refusal().reason, "command", "owner"),
            (command(), Reason::UnknownOwner, "command", "unknown_owner"),
            (
                command(),
                Reason::UnknownCommand,
                "command",
                "unknown_command",
            ),
            (
                command(),
                Reason::UnrecordedNamespace,
                "command",
                "unrecorded_namespace",
            ),
            (
                RefusalSite::Candidate,
                Reason::Unreadable {
                    role: ConfigValueRole::RunOptions,
                    message: "unknown field `prompt`".to_string(),
                },
                "candidate",
                "unreadable",
            ),
            (
                RefusalSite::Creation,
                Reason::Unreadable {
                    role: ConfigValueRole::CreationInput,
                    message: "missing field".to_string(),
                },
                "creation",
                "unreadable",
            ),
        ] {
            let refusal = lash_core::ConfigRefusal {
                owner: "probe".to_string(),
                at,
                reason,
            };
            let wire = serde_json::to_value(RemoteConfigRefusal::from(refusal.clone()))
                .expect("refusal encodes");
            assert_eq!(wire["at"]["kind"], site_kind);
            assert_eq!(wire["reason"]["kind"], reason_kind);
            assert_eq!(
                wire["reason"].get("refusal").is_some(),
                reason_kind == "owner",
                "only an owner's reason carries a refusal: {wire}"
            );
            let decoded: RemoteConfigRefusal =
                serde_json::from_value(wire).expect("refusal decodes");
            assert_eq!(lash_core::ConfigRefusal::from(decoded), refusal);
        }
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
