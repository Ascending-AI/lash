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
mod index;
mod operations;
mod persistence;
mod plugins;
mod plugins_runtime;
mod plugins_tools;
mod quickstart;
mod remote_protocol;
mod rlm;
mod streaming;
mod tools;
mod tracing;
mod worker_capacity;

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
