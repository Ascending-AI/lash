//! Compiled sources for the Rust code blocks in `docs/*.html`.
//!
//! Each module mirrors one docs page. Regions delimited by
//! `// docs:start:<id>` / `// docs:end:<id>` are embedded verbatim into the
//! page's `<pre data-snippet="<module>#<id>">` block. `scripts/lint_docs.py`
//! fails when the HTML drifts from these files (run it with `--fix-snippets`
//! to re-inject). `cargo check -p docs-snippets` catches API drift, while
//! `cargo test -p docs-snippets` executes every snippet that constructs a core
//! so incomplete host wiring fails at runtime resolution.
#![allow(dead_code, unused_variables, unused_imports)]

fn example_process_owner() -> lash::persistence::LeaseOwnerIdentity {
    static INCARNATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    lash::persistence::LeaseOwnerIdentity::opaque(
        "docs-snippets-worker",
        INCARNATION
            .get_or_init(|| uuid::Uuid::new_v4().to_string())
            .clone(),
    )
}

mod architecture_execution;
mod architecture_providers;
#[cfg(test)]
mod effect_groups;
mod embedding;
mod embedding_advanced;
mod embedding_lashlang_functions;
mod embedding_prompts;
mod embedding_turns;
mod embedding_typescript;
mod example_agent_service;
mod example_agent_workbench;
mod execution_modes;
#[cfg(test)]
mod fig1294_ingress;
#[cfg(test)]
mod fig1313_drain_policy;
#[cfg(test)]
mod fig1348_selected_drain;
mod fig1556_preflight;
mod fig1556_probe;
mod fig1922_read_session;
mod index;
mod operations;
mod persistence;
mod plugins;
mod plugins_contexts;
mod plugins_facade;
mod plugins_facade_part2;
mod plugins_facade_part3;
mod plugins_operations;
mod plugins_runtime;
mod plugins_tools;
mod quickstart;
mod remote_protocol;
mod rlm;
mod streaming;
mod tools;
mod tracing;
mod worker_capacity;

/// FIG-1921: plugin authoring is facade-complete, so an in-tree example plugin
/// is written against `lash` alone. `lash_core` is a *dev*-dependency of this
/// crate and stays reachable for snippets that document integrator seams
/// (ADR 0051); this test keeps it out of the plugin-authoring modules, where a
/// reappearance would mean the facade lost a piece of the authoring surface.
#[cfg(test)]
mod facade_only_plugin_authoring {
    const PLUGIN_MODULES: [(&str, &str); 8] = [
        ("plugins.rs", include_str!("plugins.rs")),
        ("plugins_contexts.rs", include_str!("plugins_contexts.rs")),
        ("plugins_facade.rs", include_str!("plugins_facade.rs")),
        (
            "plugins_facade_part2.rs",
            include_str!("plugins_facade_part2.rs"),
        ),
        (
            "plugins_facade_part3.rs",
            include_str!("plugins_facade_part3.rs"),
        ),
        (
            "plugins_operations.rs",
            include_str!("plugins_operations.rs"),
        ),
        ("plugins_runtime.rs", include_str!("plugins_runtime.rs")),
        ("plugins_tools.rs", include_str!("plugins_tools.rs")),
    ];

    #[test]
    fn example_plugins_need_no_lash_core_import() {
        for (module, source) in PLUGIN_MODULES {
            for (offset, line) in source.lines().enumerate() {
                let code = line.split("//").next().unwrap_or_default();
                assert!(
                    !code.contains("lash_core"),
                    "examples/docs-snippets/src/{module}:{} names lash_core: {}. \
                     A plugin example must compile against `lash` alone; add the missing \
                     authoring type to lash::plugins, or record it as an integrator seam \
                     in ADR 0051.",
                    offset + 1,
                    line.trim()
                );
            }
        }
    }

    /// `PLUGIN_MODULES` is hand-written, so it can silently miss a fifth plugin
    /// example — which would then be free to import `lash_core`. Hold the list
    /// to what is actually on disk.
    #[test]
    fn the_plugin_module_list_covers_every_plugin_example() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
            .map(|entry| entry.expect("read dir entry").file_name())
            .filter_map(|name| name.to_str().map(str::to_owned))
            .filter(|name| name.starts_with("plugins") && name.ends_with(".rs"))
            .collect();
        on_disk.sort();

        let mut listed: Vec<String> = PLUGIN_MODULES
            .iter()
            .map(|(module, _)| (*module).to_owned())
            .collect();
        listed.sort();

        assert_eq!(
            listed, on_disk,
            "PLUGIN_MODULES has drifted from examples/docs-snippets/src: every \
             plugins*.rs module must be listed so the no-lash_core rule covers it."
        );
    }
}

#[cfg(test)]
mod test_support {
    pub(crate) fn provider() -> lash::provider::ProviderHandle {
        lash::provider::ProviderHandle::unconfigured()
    }

    pub(crate) fn model() -> lash::ModelSpec {
        lash::ModelSpec::builder("docs-snippet-test")
            .context_window_tokens(4_096)
            .build()
            .expect("valid docs-snippet test model")
    }

    pub(crate) fn assert_builder_resolved(result: anyhow::Result<()>) {
        if let Err(error) = result {
            assert!(
                error.downcast_ref::<lash::EmbedError>().is_none(),
                "documentation builder did not resolve: {error:#}"
            );
        }
    }
}
