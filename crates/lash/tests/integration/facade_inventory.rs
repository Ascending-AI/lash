//! Compile witness for the ADR 0079 facade: every path the host (figments)
//! imports from a `lash-internal-*` crate, reached through `lash::` alone.
//!
//! The inventory is the set of distinct paths recorded in the FIG-3189
//! evidence sweep of the host tree. A path that names an inherent associated
//! item (`Type::new`, `Type::default`) or an enum variant
//! (`Enum::Variant`) is witnessed by its owning type: the facade decides where
//! the type lives, not what hangs off it. Paths that no longer exist anywhere
//! in this workspace (the host pins an older revision) are listed in the pull
//! request rather than witnessed here.
//!
//! Imports bind to `_` on purpose: this file proves that the paths resolve,
//! and binding no names keeps it free of ordering and shadowing accidents.
//!
//! `as _` silences `unused_imports` only for a trait, whose methods enter
//! scope through the anonymous binding. Most rows here name a struct, an
//! enum or a function, so the binding is genuinely unused and the lint
//! fires: without the crate-level allow below, `//:workspace_clippy`
//! (`-Dwarnings`) reports 152 errors. The allow covers `unused_imports`
//! alone; a path that stops resolving is still E0432, a hard error.

#![allow(unused_imports)]
// --- Ungated facade: lash-internal-core, -sansio, -trace, -remote-protocol ---

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash::Backend as _;
use lash::ModelSpec as _;
use lash::PendingTurnInput as _;
use lash::PendingTurnInputCancelOutcome as _;
use lash::PendingTurnInputSuffixCancelOutcome as _;
use lash::TurnActivity as _;
use lash::TurnCancelRequest as _;
use lash::TurnEvent as _;
use lash::TurnOutcome as _;
use lash::TurnStop as _;
use lash::TurnWorkDriver as _;
use lash::attachments::AttachmentCreateMeta as _;
use lash::attachments::AttachmentId as _;
use lash::attachments::AttachmentRef as _;
use lash::attachments::MediaType as _;
use lash::attachments::content_id as _;
use lash::direct::GenerationOptions as _;
use lash::direct::GenerationOptions as _;
use lash::direct::LlmOutputPart as _;
use lash::direct::LlmTerminalReason as _;
use lash::direct::LlmUsage as _;
use lash::durability::BoundaryReason as _;
use lash::durability::StoreSet as _;
use lash::durability::ensure_durable_effect_input as _;
use lash::observe::InMemoryLiveReplayStore as _;
use lash::persistence::CheckpointKind as _;
use lash::persistence::DurabilityTier as _;
use lash::persistence::PendingTurnInputDraft as _;
use lash::persistence::SessionAttachmentStore as _;
use lash::persistence::SessionRelation as _;
use lash::persistence::SessionStoreCreateRequest as _;
use lash::persistence::TurnInputCheckpointBoundary as _;
use lash::persistence::TurnInputIngress as _;
use lash::persistence::TurnInputState as _;
use lash::persistence::reclaim_unreferenced_attachments as _;
use lash::plugins::ContextError as _;
use lash::plugins::PluginError as _;
use lash::plugins::PluginOptions as _;
use lash::plugins::PreparedContext as _;
use lash::plugins::SegmentHandover as _;
use lash::plugins::ToolCatalog as _;
use lash::plugins::TurnContextTransform as _;
use lash::plugins::TurnTransformContext as _;
use lash::process::CausalRef as _;
use lash::process::ProcessAwaitOutput as _;
use lash::process::ProcessCompletionAuthority as _;
use lash::process::ProcessEventAppendRequest as _;
use lash::process::ProcessExecutionEnvRef as _;
use lash::process::ProcessExecutionEnvSpec as _;
use lash::process::ProcessIdentity as _;
use lash::process::ProcessInput as _;
use lash::process::ProcessListFilter as _;
use lash::process::ProcessOriginator as _;
use lash::process::ProcessProvenance as _;
use lash::process::ProcessPruneReport as _;
use lash::process::ProcessRecord as _;
use lash::process::ProcessRef as _;
use lash::process::ProcessRegistration as _;
use lash::process::ProcessStartRequest as _;
use lash::process::ProcessStatusFilter as _;
use lash::process::SessionScope as _;
use lash::provider::CacheControlDialect as _;
use lash::provider::HostNamespace as _;
use lash::provider::LlmContentBlock as _;
use lash::provider::LlmJsonSchema as _;
use lash::provider::LlmRequest as _;
use lash::provider::LlmRequest as _;
use lash::provider::LlmRequestScope as _;
use lash::provider::LlmResponse as _;
use lash::provider::LlmToolChoice as _;
use lash::provider::ModelCapability as _;
use lash::provider::ModelCapability as _;
use lash::provider::ModelEffortValidationCategory as _;
use lash::provider::ProviderCompletion as _;
use lash::provider::ProviderComponents as _;
use lash::provider::ProviderFailureKind as _;
use lash::provider::ProviderReliability as _;
use lash::provider::ReasoningCapability as _;
use lash::provider::ReasoningDisableEncoding as _;
use lash::provider::ReasoningSelection as _;
use lash::provider::ReasoningSelection as _;
use lash::provider::StreamTermination as _;
use lash::secrets::Redacted as _;
// `async_trait` is an attribute macro re-exported at the facade root so
// `#[async_trait]` facade traits can be implemented without a host-side
// `async-trait` dependency.
use lash::async_trait as _;
use lash::remote::llm::RemoteSchemaContract as _;
use lash::remote::llm::RemoteSchemaProjectionPolicy as _;
use lash::remote::processes::RemoteProcessIdentity as _;
use lash::remote::processes::RemoteProcessRecord as _;
use lash::runtime::AdmittedScopeError as _;
use lash::runtime::Clock as _;
use lash::runtime::RuntimeEffectController as _;
use lash::runtime::RuntimeError as _;
use lash::runtime::RuntimeErrorCode as _;
use lash::runtime::ScopedEffectController as _;
use lash::runtime::SessionPolicy as _;
use lash::runtime::SystemClock as _;
use lash::runtime::current_epoch_ms as _;
use lash::tools::ToolId as _;
use lash::tools::ToolProvider as _;
use lash::tools::ToolRegistry as _;
use lash::tools::ToolRetryPolicy as _;
use lash::tools::ToolSourceHandle as _;
use lash::tools::invalid_tool_args as _;
use lash::tools::object_schema as _;
use lash::tools::parse_optional_usize_arg as _;
use lash::tracing::TraceContext as _;
use lash::tracing::TracePromptComponent as _;
use lash::tracing::TraceTokenUsage as _;
use lash::tracing::TraceToolCallOutcome as _;
use lash::tracing::TraceToolCallOutput as _;
use lash::tracing::TraceToolCallStatus as _;
use lash::tracing::TraceTurnCompletionReason as _;
use lash::tracing::TraceTurnFailureReason as _;
use lash::tracing::TraceTurnOutcome as _;
use lash::triggers::LashSchema as _;
use lash::triggers::TriggerDeliveryReservation as _;
use lash::triggers::TriggerInputBinding as _;
use lash::triggers::TriggerOccurrenceFilter as _;
use lash::triggers::TriggerOccurrenceRecord as _;
use lash::triggers::TriggerOccurrenceRequest as _;
use lash::triggers::TriggerRegistration as _;
use lash::triggers::TriggerSubscriptionDraft as _;
use lash::triggers::TriggerSubscriptionFilter as _;
use lash::triggers::TriggerSubscriptionRecord as _;

/// `lash_sansio::schema_contract`, the one sans-io module the host names.
use lash::schema::{SchemaContract as _, SchemaProjectionPolicy as _};

// --- `rlm`: the Lashlang protocol, runtime and language surface ---

#[cfg(feature = "rlm")]
mod rlm_inventory {
    use lash::persistence::LashlangArtifacts as _;
    use lash::rlm::LashlangAbilities as _;
    use lash::rlm::LashlangHostEnvironment as _;
    use lash::rlm::LinkedModule as _;
    use lash::rlm::ModuleCompileOutput as _;
    use lash::rlm::NamedDataType as _;
    use lash::rlm::TypeExpr as _;
    use lash::rlm::TypeField as _;
    use lash::rlm::lashlang_surface_extension as _;
    use lash::tools::link_with_deferred_resolution as _;

    // The Lashlang language vocabulary, re-exported whole as `lash::rlm::lang`.
    use lash::rlm::lang::AbilityOp as _;
    use lash::rlm::lang::AbilityResult as _;
    use lash::rlm::lang::ContentHash as _;
    use lash::rlm::lang::Entry as _;
    use lash::rlm::lang::ExecutionEnvironment as _;
    use lash::rlm::lang::ExecutionHost as _;
    use lash::rlm::lang::ExecutionHostError as _;
    use lash::rlm::lang::Expr as _;
    use lash::rlm::lang::HostDescriptor as _;
    use lash::rlm::lang::HostRequirementsRef as _;
    use lash::rlm::lang::ImageValue as _;
    use lash::rlm::lang::ListComprehensionClause as _;
    use lash::rlm::lang::ModuleCompileRequest as _;
    use lash::rlm::lang::ModuleIntrospection as _;
    use lash::rlm::lang::ModuleRef as _;
    use lash::rlm::lang::NamedDataTypeIntrospection as _;
    use lash::rlm::lang::ProcessIntrospection as _;
    use lash::rlm::lang::ResourceOperation as _;
    use lash::rlm::lang::ResourceOperationBatchResult as _;
    use lash::rlm::lang::ResourceOperationResult as _;
    use lash::rlm::lang::State as _;
    use lash::rlm::lang::TriggerInputTemplate as _;
    use lash::rlm::lang::TriggerListRequest as _;
    use lash::rlm::lang::TriggerRegistrationRequest as _;
    use lash::rlm::lang::Value as _;
    use lash::rlm::lang::add_trigger_resource_operations as _;
    use lash::rlm::lang::compile as _;
    use lash::rlm::lang::compile_module as _;
    use lash::rlm::lang::execute as _;
    use lash::rlm::lang::from_json as _;
}

// --- `testing`: embedder test helpers ---

#[cfg(feature = "testing")]
mod testing_inventory {
    use lash::testing::TestProvider as _;
    use lash::testing::code_execution_context as _;
    use lash::testing::exec_code_invocation as _;
    use lash::testing::mock_tool_context_with_execution_binding as _;
    use lash::testing::store_fixtures::authorize_completion_deferral_for_test as _;
    use lash::testing::tool_registry_with_live_provider as _;
}

#[cfg(all(feature = "rlm", feature = "testing"))]
mod rlm_testing_inventory {
    // The runtime rebuild certification the host runs, reached through the
    // facade's own `testing` module. The durable-store laws are not facade
    // surface: a host depends on `lash-internal-conformance` directly.
    use lash::testing::runtime_rebuild_and_worker_recovery as _;

    use lash::rlm::lang::testing::conformance::ReopenableLashlangArtifactStore as _;
    use lash::testing::deferred_resolution_link_key as _;
    // The host names `lashlang_artifact_store_reopenable`; the live name of the
    // same conformance entry point is `survives_reopen`.
    use lash::rlm::lang::testing::conformance::survives_reopen as _;
}

// --- Host-wired extension features: one module per feature ---

#[cfg(feature = "sqlite")]
mod sqlite_inventory {
    use lash::sqlite::SqliteDatabase as _;
    use lash::sqlite::SqliteSessionStoreFactory as _;
    // ADR 0102's zero-infra entry point: one backend, file or memory.
    use lash::sqlite::{SqliteBackend as _, SqliteBackendOptions as _, SqliteLocation as _};
    // The store set a Restate backend journals its effects beside.
    use lash::sqlite::SqliteStoreSet as _;
}

#[cfg(feature = "postgres")]
mod postgres_inventory {
    use lash::postgres::PostgresSessionStoreFactory as _;
    use lash::postgres::PostgresStorage as _;
    use lash::postgres::PostgresStoreSet as _;
}

#[cfg(feature = "s3")]
mod s3_inventory {
    use lash::s3::S3AttachmentStore as _;
    use lash::s3::S3AttachmentStoreConfig as _;
}

#[cfg(feature = "restate")]
mod restate_inventory {
    use lash::restate::RestateEffectHost as _;
    use lash::restate::RestateEngine as _;
}

#[cfg(feature = "openai")]
mod openai_inventory {
    use lash::openai::OpenAiCompatibleProvider as _;
    use lash::openai::OpenAiProvider as _;
}

#[cfg(feature = "anthropic")]
mod anthropic_inventory {
    use lash::anthropic::AnthropicProvider as _;
}

#[cfg(feature = "google")]
mod google_inventory {
    use lash::google::GoogleOAuthProvider as _;
}

#[cfg(feature = "mcp")]
mod mcp_inventory {
    use lash::mcp::McpError as _;
    use lash::mcp::McpServerConfig as _;
}

#[cfg(feature = "subagents")]
mod subagents_inventory {
    use lash::subagents::SubagentsPluginFactory as _;
}

#[cfg(feature = "typescript")]
mod typescript_inventory {
    use lash::typescript::{link as _, parse as _};
}

#[cfg(feature = "http-transport")]
mod http_transport_inventory {
    use lash::http_transport::ReqwestClient as _;
}

// --- Whole-module coverage (FIG-3205) ---
//
// The `use` witnesses above prove that named facade paths resolve. The tests
// below prove the completeness direction for the surfaces the facade promises
// wholesale: every public item of a covered leaf module is re-exported, and
// every type reachable from a public `TraceEvent` field is nameable through
// the facade. Both sets are derived from source at test time — not from a
// hand-maintained list — so the next upstream `pub` item that lacks a facade
// home fails here, in lash CI, instead of in a host's pin bump.
//
// The parsers are deliberately small and textual, matching the style of
// `tests/integration/one_home.rs`; each test asserts the size of the set it derived so a
// broken parser fails loudly instead of vacuously passing.

mod whole_module_coverage {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    fn read(rel: &str) -> String {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
    }

    /// Drop `//` line comments (including doc comments) so `pub` inside prose
    /// or commented-out code is never mistaken for a real item.
    fn uncommented(src: &str) -> String {
        src.lines()
            .map(|l| {
                if l.trim_start().starts_with("//") {
                    ""
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn ident_at(s: &str) -> String {
        s.chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect()
    }

    /// Byte position of the character `depth` levels of `{`/`}` above `pos` is
    /// computed over — i.e. brace depth at a byte offset.
    fn depth_at(src: &str, pos: usize) -> usize {
        src.as_bytes()[..pos].iter().fold(0usize, |d, &b| match b {
            b'{' => d + 1,
            b'}' => d.saturating_sub(1),
            _ => d,
        })
    }

    /// `(position, body)` of every `pub use ...;` statement, where `body` is
    /// the verbatim text between `pub use` and the terminating `;`.
    fn pub_use_statements(src: &str) -> Vec<(usize, String)> {
        let bytes = src.as_bytes();
        let mut out = Vec::new();
        let mut search = 0;
        while let Some(rel) = src[search..].find("pub use ") {
            let start = search + rel;
            let line_start = src[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
            if src[line_start..start].trim_start().starts_with("//") {
                search = start + 8;
                continue;
            }
            let mut j = start;
            while j < bytes.len() && bytes[j] != b';' {
                j += 1;
            }
            out.push((start, src[start + 8..j].to_string()));
            search = j + 1;
        }
        out
    }

    /// Leaf names and glob prefixes introduced by one `pub use` body.
    /// `pub use a::b::{c, d as e};` yields leaves `c`, `e`; `pub use a::*`
    /// yields glob `a`. A braced glob `a::{b, c::*}` records glob `a::c`.
    fn use_leaves_and_globs(body: &str) -> (Vec<String>, Vec<String>) {
        let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut leaves = Vec::new();
        let mut globs = Vec::new();
        // Split top-level commas first, then pull `{`-prefixed tails apart.
        let mut segments: Vec<String> = Vec::new();
        let mut prefix_stack: Vec<String> = Vec::new();
        for raw in flat.split(',') {
            let mut seg = raw.trim().to_string();
            while let Some(idx) = seg.find('{') {
                let head = seg[..idx].trim_end().to_string();
                if !head.is_empty() {
                    prefix_stack.push(head.clone());
                    seg = format!("{}{}", head, seg[idx + 1..].trim());
                } else {
                    seg = seg[idx + 1..].trim().to_string();
                }
            }
            while seg.ends_with('}') {
                seg.pop();
                prefix_stack.pop();
                seg = seg.trim_end().to_string();
            }
            if !seg.is_empty() {
                segments.push(seg);
            }
        }
        for seg in segments {
            let seg = seg.trim_end_matches(';').trim().to_string();
            if let Some(prefix) = seg.strip_suffix("::*") {
                globs.push(prefix.trim().to_string());
                continue;
            }
            let leaf = if let Some(idx) = seg.find(" as ") {
                seg[idx + 4..].trim().to_string()
            } else {
                seg.rsplit("::").next().unwrap_or(&seg).trim().to_string()
            };
            if leaf
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic() || c == '_')
                .unwrap_or(false)
                && leaf.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                leaves.push(leaf);
            }
        }
        (leaves, globs)
    }

    /// Public item names a leaf module file exposes: top-level
    /// `pub struct|enum|trait|type|union|static|const|mod|fn` declarations plus
    /// top-level `pub use` leaves. Items inside `impl`/`fn`/inline-module
    /// bodies (brace depth > 0) are not module items and are excluded, which
    /// also keeps `#[cfg(test)]` helper modules out of the inventory.
    fn public_item_names(src: &str) -> BTreeSet<String> {
        let src = uncommented(src);
        let mut out = BTreeSet::new();
        for (pos, body) in pub_use_statements(&src) {
            if depth_at(&src, pos) == 0 {
                let (leaves, _) = use_leaves_and_globs(&body);
                out.extend(leaves);
            }
        }
        const KEYWORDS: [&str; 9] = [
            "struct", "enum", "trait", "type", "union", "static", "const", "mod", "fn",
        ];
        let bytes = src.as_bytes();
        let mut search = 0;
        while let Some(rel) = src[search..].find("pub ") {
            let start = search + rel;
            if start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
                search = start + 4;
                continue;
            }
            let rest = src[start + 4..].trim_start();
            if rest.starts_with('(') || depth_at(&src, start) != 0 {
                search = start + 4;
                continue; // pub(crate) / not a module-level item
            }
            for kw in KEYWORDS {
                if let Some(after) = rest.strip_prefix(kw) {
                    let name = ident_at(after.trim_start());
                    if !name.is_empty() {
                        out.insert(name);
                    }
                    break;
                }
            }
            search = start + 4;
        }
        out
    }

    /// Public names of a leaf module, resolving `pub use local_mod::*` globs
    /// recursively into sibling files (`dir/<mod>.rs`, `dir/<mod>/mod.rs`).
    /// Globs into external crates cannot be enumerated textually; they are
    /// returned separately so a new one fails loudly.
    fn module_public_names(src: &str, dir: &Path) -> (BTreeSet<String>, Vec<String>) {
        let src = uncommented(src);
        let mut names = public_item_names(&src);
        let mut external_globs = Vec::new();
        for (pos, body) in pub_use_statements(&src) {
            if depth_at(&src, pos) != 0 {
                continue;
            }
            let (_, globs) = use_leaves_and_globs(&body);
            for g in globs {
                let seg = g.rsplit("::").next().unwrap_or(&g).to_string();
                let file = dir.join(format!("{seg}.rs"));
                let modrs = dir.join(&seg).join("mod.rs");
                if file.exists() {
                    let sub = std::fs::read_to_string(&file)
                        .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
                    let sub_dir = file
                        .parent()
                        .unwrap_or_else(|| panic!("{} has no parent", file.display()))
                        .join(&seg);
                    let (sub_names, sub_ext) = module_public_names(&sub, &sub_dir);
                    names.extend(sub_names);
                    external_globs.extend(sub_ext);
                } else if modrs.exists() {
                    let sub = std::fs::read_to_string(&modrs)
                        .unwrap_or_else(|e| panic!("read {}: {e}", modrs.display()));
                    let (sub_names, sub_ext) = module_public_names(&sub, &dir.join(&seg));
                    names.extend(sub_names);
                    external_globs.extend(sub_ext);
                } else {
                    external_globs.push(g);
                }
            }
        }
        (names, external_globs)
    }

    /// Facade re-exports grouped by module home: `(leaf names, glob prefixes)`
    /// per enclosing `pub mod` path in `src/lib.rs` (`"root"` at file scope).
    /// Mirrors the `collect` logic of `tests/integration/one_home.rs`.
    fn facade_exports() -> BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> {
        let src = uncommented(&read("src/lib.rs"));
        let bytes = src.as_bytes();

        // Inline `pub mod NAME { .. }` spans.
        let mut ranges: Vec<(String, usize, usize)> = Vec::new();
        let mut search = 0;
        while let Some(rel) = src[search..].find("pub mod ") {
            let start = search + rel;
            let name = ident_at(&src[start + 8..]);
            let mut j = start + 8 + name.len();
            while j < bytes.len() && bytes[j] != b'{' && bytes[j] != b';' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'{' {
                let mut depth = 0usize;
                let mut k = j;
                while k < bytes.len() {
                    match bytes[k] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                }
                ranges.push((name, start, k));
            }
            search = start + 8;
        }

        let module_for = |pos: usize| -> String {
            let mut chain: Vec<&(String, usize, usize)> =
                ranges.iter().filter(|r| r.1 <= pos && pos <= r.2).collect();
            chain.sort_by_key(|r| r.1);
            if chain.is_empty() {
                "root".to_string()
            } else {
                chain
                    .iter()
                    .map(|r| r.0.as_str())
                    .collect::<Vec<_>>()
                    .join("::")
            }
        };

        let mut out: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
        for (pos, body) in pub_use_statements(&src) {
            let (leaves, globs) = use_leaves_and_globs(&body);
            let entry = out.entry(module_for(pos)).or_default();
            entry.0.extend(leaves);
            entry.1.extend(globs);
        }
        out
    }

    /// Assert `facade_module` re-exports every public item of the leaf module
    /// at `leaf_file`, either by name or through a `::*` glob of `leaf_path`.
    fn assert_module_covered(
        exports: &BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)>,
        leaf_file: &str,
        leaf_dir: &str,
        leaf_path: &str,
        facade_module: &str,
        min_items: usize,
    ) {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(leaf_dir);
        let (names, external_globs) = module_public_names(&read(leaf_file), &dir);
        assert!(
            external_globs.is_empty(),
            "{leaf_path} gained a `pub use` glob into an external crate \
             ({external_globs:?}) — extend this test to enumerate it",
        );
        assert!(
            names.len() >= min_items,
            "{leaf_path} parser found only {} items — the parser likely broke",
            names.len(),
        );
        let (leaves, globs) = exports.get(facade_module).cloned().unwrap_or_default();
        if globs.contains(leaf_path) {
            return; // whole-module glob: coverage holds by construction
        }
        let missing: Vec<String> = names.difference(&leaves).cloned().collect();
        assert!(
            missing.is_empty(),
            "lash::{facade_module} does not re-export every public item of \
             {leaf_path}; missing: {missing:?}",
        );
    }

    #[test]
    fn facade_covers_whole_leaf_modules() {
        let exports = facade_exports();
        assert!(
            exports.values().map(|v| v.0.len()).sum::<usize>() > 100,
            "facade export parser found almost nothing — the parser likely broke",
        );

        assert_module_covered(
            &exports,
            "../lash-remote-protocol/src/processes.rs",
            "../lash-remote-protocol/src/processes",
            "lash_remote_protocol::processes",
            "remote::processes",
            60,
        );
        assert_module_covered(
            &exports,
            "../lash-remote-protocol/src/triggers.rs",
            "../lash-remote-protocol/src/triggers",
            "lash_remote_protocol::triggers",
            "remote::triggers",
            20,
        );
        assert_module_covered(
            &exports,
            "../lash-tool-support/src/lib.rs",
            "../lash-tool-support/src",
            "lash_tool_support",
            "tools",
            5,
        );
        assert_module_covered(
            &exports,
            "../lash-sansio/src/redacted.rs",
            "../lash-sansio/src",
            "lash_sansio",
            "secrets",
            1,
        );
    }

    /// The body of each `pub struct|enum|type` in `src`, used to walk field
    /// types transitively.
    fn item_bodies(src: &str) -> BTreeMap<String, String> {
        let src = uncommented(src);
        let bytes = src.as_bytes();
        let mut out = BTreeMap::new();
        for kw in ["struct", "enum", "type"] {
            let needle = format!("pub {kw} ");
            let mut search = 0;
            while let Some(rel) = src[search..].find(&needle) {
                let start = search + rel;
                if start > 0
                    && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_')
                {
                    search = start + needle.len();
                    continue;
                }
                let rest = &src[start + needle.len()..];
                let name = ident_at(rest);
                let after = &rest[name.len()..];
                let abytes = after.as_bytes();
                let mut i = 0;
                while i < abytes.len()
                    && abytes[i] != b'{'
                    && abytes[i] != b'('
                    && abytes[i] != b';'
                    && abytes[i] != b'='
                {
                    i += 1;
                }
                if i < abytes.len() && (abytes[i] == b'{' || abytes[i] == b'(') {
                    let (open, close) = if abytes[i] == b'{' {
                        (b'{', b'}')
                    } else {
                        (b'(', b')')
                    };
                    let mut depth = 0usize;
                    let mut k = i;
                    while k < abytes.len() {
                        if abytes[k] == open {
                            depth += 1;
                        } else if abytes[k] == close {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        k += 1;
                    }
                    out.insert(name, after[i..=k.min(abytes.len() - 1)].to_string());
                } else if i < abytes.len() && abytes[i] == b'=' {
                    let mut k = i;
                    while k < abytes.len() && abytes[k] != b';' {
                        k += 1;
                    }
                    out.insert(name, after[i..k].to_string());
                }
                search = start + needle.len();
            }
        }
        out
    }

    #[test]
    fn trace_event_reachable_types_are_nameable() {
        // Every type reachable from a public `TraceEvent` field must be
        // exported somewhere in the facade: a consumer that can match on the
        // variant but cannot name the payload type cannot write a function
        // over it or build one in a test.
        let src = read("../lash-trace/src/lib.rs");
        let bodies = item_bodies(&src);
        let mut defined: BTreeSet<String> = bodies.keys().cloned().collect();
        // `pub use` re-exports name types defined in sibling crates; they are
        // reachable and must have a facade home, but their own fields are out
        // of scope for this walk.
        for (_, body) in pub_use_statements(&uncommented(&src)) {
            let (leaves, _) = use_leaves_and_globs(&body);
            defined.extend(leaves);
        }

        let mut seen = BTreeSet::new();
        let mut stack = vec!["TraceEvent".to_string()];
        while let Some(n) = stack.pop() {
            if !seen.insert(n.clone()) {
                continue;
            }
            if let Some(body) = bodies.get(&n) {
                for token in body
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .filter(|t| {
                        t.chars()
                            .next()
                            .map(|c| c.is_ascii_uppercase())
                            .unwrap_or(false)
                    })
                {
                    if defined.contains(token) && !seen.contains(token) {
                        stack.push(token.to_string());
                    }
                }
            }
        }

        // Sanity: the walk found the real payload vocabulary, including the
        // types this test exists to keep exported.
        for expected in [
            "TraceTurnOutcome",
            "TraceTurnCompletionReason",
            "TraceTurnFailureReason",
            "TraceToolCallStatus",
            "TraceExecToolCall",
            "TraceRetryAttempt",
        ] {
            assert!(
                seen.contains(expected),
                "reachability walk missed {expected} — the parser likely broke",
            );
        }
        assert!(seen.len() >= 30, "reachable set too small: {seen:?}");

        let exports = facade_exports();
        let all_leaves: BTreeSet<String> = exports
            .values()
            .flat_map(|(leaves, _)| leaves.iter().cloned())
            .collect();
        let missing: Vec<String> = seen.difference(&all_leaves).cloned().collect();
        assert!(
            missing.is_empty(),
            "types reachable from `TraceEvent` have no facade home: {missing:?}",
        );
    }
}
