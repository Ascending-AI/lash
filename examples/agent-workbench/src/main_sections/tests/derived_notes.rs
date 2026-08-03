#[cfg(test)]
mod derived_notes_tests {
    use super::tests::{explicit_durable_test_facets, run_async_test_on_stack_budget, text_response};
    use super::*;

    /// Both halves of the derive-then-append fence the workbench annotator relies
    /// on, driven through real turns against a durable store.
    ///
    /// The annotator always writes a note back one commit late, so its base is
    /// never the head it lands on. That is fine and must stay fine — the summary is
    /// still true of the prefix it read. What must *not* be fine is writing it into
    /// a session that has since been rewound onto a different line of history: the
    /// conversation the note describes is not the one this session is having any
    /// more.
    #[test]
    fn derived_notes_survive_an_advanced_head_and_are_dropped_by_a_rewind() {
        run_async_test_on_stack_budget("workbench-derived-notes", || {
            derived_notes_survive_an_advanced_head_and_are_dropped_by_a_rewind_inner()
        });
    }

    async fn derived_notes_survive_an_advanced_head_and_are_dropped_by_a_rewind_inner() {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-derived-notes-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create derived-notes dir");
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-derived-notes")
            .complete(|_| async { Ok(text_response("<lashlang>\nfinish \"answered\"\n</lashlang>")) })
            .build()
            .into_handle();
        let plugin = Arc::new(WorkbenchPluginFactory::new(""));
        let notes = plugin.derived_notes();
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(
                lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None)
                    .expect("model spec"),
            )
            .store_factory(Arc::clone(&store_factory))
            .plugin(plugin as Arc<dyn PluginFactory>)
            .build()
            .expect("build derived-notes core");

        let session = core
            .session("workbench-derived-notes")
            .open()
            .await
            .expect("open the annotated session");
        run_derived_notes_turn(&session, "first question").await;
        let first_leaf = derived_notes_leaf(&session).await;
        // The operator marks this turn as a branch point while it is still the
        // head; that retention is what a later rewind forks from.
        core.pin(&first_leaf)
            .await
            .expect("retain the first turn as a branch point");
        assert!(
            notes.settled().is_empty(),
            "the first turn's summary is still being derived; nothing has been \
             written back yet"
        );

        // The head moves on while the first note is in flight. The note is still
        // true of the prefix it read, so it must be kept.
        run_derived_notes_turn(&session, "second question").await;
        // The write-back lands after the turn's own commit, so read it back the
        // way a restarted host would: from durable storage.
        session.close().await.expect("close the annotated session");
        let session = core
            .session("workbench-derived-notes")
            .open()
            .await
            .expect("reopen the annotated session");
        let graph = derived_notes_graph(&session).await;
        let settled = notes.settled();
        let [written] = settled.as_slice() else {
            panic!("exactly one note has been written back: {settled:?}");
        };
        let WorkbenchSettledNote::Written {
            base_node_id,
            node_id,
            leaf_node_id,
        } = written.clone()
        else {
            panic!("an advanced head must keep the derivation: {written:?}");
        };
        assert_eq!(base_node_id, first_leaf);
        assert_eq!(graph.leaf_node_id.as_deref(), Some(node_id.as_str()));
        assert_eq!(leaf_node_id, node_id);
        let note = graph.find_node(&node_id).expect("the note is durable");
        assert_ne!(
            note.parent_node_id.as_deref(),
            Some(first_leaf.as_str()),
            "the note parents on the current leaf, not on the base it required: \
             its graph position carries no claim about what it was derived from"
        );
        assert_eq!(
            derived_note_base(note),
            Some(first_leaf.as_str()),
            "which is why the base rides in the payload instead"
        );
        let active_path = active_path_nodes(&graph)
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        let base_at = active_path
            .iter()
            .position(|candidate| *candidate == first_leaf)
            .expect("the base is still on the active path");
        let note_at = active_path
            .iter()
            .position(|candidate| *candidate == node_id)
            .expect("the note is on the active path");
        assert!(
            note_at > base_at + 1,
            "the second turn's nodes must sit between the base and the note, with \
             nothing lost or reordered: {active_path:?}"
        );
        // The base of the note now in flight: the head the second turn committed.
        let second_leaf = note
            .parent_node_id
            .clone()
            .expect("the note has a parent leaf");
        session.close().await.expect("close the annotated session");

        // An operator rewinds the conversation to the first turn. Under ADR 0047
        // that retains the node and continues from it as a new session, so the
        // second turn's line of history is abandoned — and the note still in
        // flight was derived from it.
        core.fork_at(first_leaf.clone(), "workbench-derived-notes-rewound")
            .await
            .expect("rewind the conversation to the first turn");
        let rewound = core
            .session("workbench-derived-notes-rewound")
            .open()
            .await
            .expect("open the rewound session");
        run_derived_notes_turn(&rewound, "different second question").await;

        assert_eq!(
            notes.settled().last(),
            Some(&WorkbenchSettledNote::AbandonedBranch {
                base_node_id: second_leaf.clone(),
            }),
            "the in-flight note described a branch this session no longer \
             executes, so the fence must refuse it"
        );
        let rewound_graph = derived_notes_graph(&rewound).await;
        assert!(
            !rewound_graph.active_path_contains(&second_leaf),
            "the rewound session really did abandon the note's base"
        );
        assert!(
            active_path_nodes(&rewound_graph)
                .iter()
                .all(|node| derived_note_base(node) != Some(second_leaf.as_str())),
            "and nothing derived from it was written: a refused append writes \
             nothing at all"
        );
        rewound.close().await.expect("close the rewound session");
        let _ = std::fs::remove_dir_all(data_dir);
    }

    async fn run_derived_notes_turn(session: &lash::LashSession, text: &str) {
        session
            .turn(lash::TurnInput::text(text))
            .turn_id(format!("workbench-derived-notes:{}", uuid::Uuid::new_v4()))
            .run()
            .await
            .unwrap_or_else(|error| panic!("run derived-notes turn `{text}`: {error:?}"));
    }

    async fn derived_notes_graph(session: &lash::LashSession) -> lash::persistence::SessionGraph {
        session.admin().state().export().await.session_graph
    }

    async fn derived_notes_leaf(session: &lash::LashSession) -> String {
        derived_notes_graph(session)
            .await
            .leaf_node_id
            .clone()
            .expect("the session has a committed leaf")
    }

    fn derived_note_base(node: &lash::persistence::SessionNodeRecord) -> Option<&str> {
        let lash_core::SessionNodePayload::Plugin { plugin_type, body } = &node.payload else {
            return None;
        };
        if plugin_type != WORKBENCH_DERIVED_NOTE_PLUGIN_TYPE {
            return None;
        }
        body.as_ref()
            .get("derived_from_node_id")
            .and_then(Value::as_str)
    }

    fn active_path_nodes(
        graph: &lash::persistence::SessionGraph,
    ) -> Vec<&lash::persistence::SessionNodeRecord> {
        let mut path = Vec::new();
        let mut node_id = graph.leaf_node_id.as_deref();
        while let Some(id) = node_id {
            let node = graph.find_node(id).expect("active path node exists");
            path.push(node);
            node_id = node.parent_node_id.as_deref();
        }
        path.reverse();
        path
    }
}
