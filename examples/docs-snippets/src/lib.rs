//! Compiled sources for the Rust code blocks in `docs/*.html`.
//!
//! Each module mirrors one docs page. Regions delimited by
//! `// docs:start:<id>` / `// docs:end:<id>` are embedded verbatim into the
//! page's `<pre data-snippet="<module>#<id>">` block. `scripts/lint_docs.py`
//! fails when the HTML drifts from these files (run it with `--fix-snippets`
//! to re-inject), and `cargo check -p docs-snippets` fails when a snippet
//! stops compiling against the current API.
#![allow(dead_code, unused_variables, unused_imports)]

mod architecture_execution;
mod architecture_providers;
mod embedding;
mod embedding_advanced;
mod embedding_prompts;
mod embedding_turns;
mod example_agent_service;
mod example_agent_workbench;
mod execution_modes;
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
