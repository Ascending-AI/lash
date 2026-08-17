pub(crate) mod lashlang;
pub(crate) mod typescript;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::{ExecRequest, ExecResponse, RuntimeExecutionContext, SessionError};
use lash_rlm_types::RlmGlobalsPatchPluginBody;

pub(crate) use lashlang::{LashlangDialect, LashlangDialectServices};
pub(crate) use typescript::TypescriptDialect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CellTags {
    pub(crate) open: &'static str,
    pub(crate) close: &'static str,
}

pub(crate) struct BoundVariablesPromptRender {
    render: Box<dyn FnOnce() -> Arc<str> + Send>,
}

impl BoundVariablesPromptRender {
    pub(crate) fn new(render: impl FnOnce() -> Arc<str> + Send + 'static) -> Self {
        Self {
            render: Box::new(render),
        }
    }

    pub(crate) fn render(self) -> Arc<str> {
        (self.render)()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum DialectRegistryError {
    #[error("RLM language `{language}` is not registered")]
    Unregistered { language: String },
    #[error("RLM language `{language}` is registered but session language `{active}` is pinned")]
    Inactive { language: String, active: String },
}

#[async_trait::async_trait]
pub(crate) trait RlmDialectSession: Send {
    async fn execute(
        &mut self,
        ctx: RuntimeExecutionContext<'_>,
        request: ExecRequest,
        session_projected_bindings: crate::projection::RlmProjectedBindings,
    ) -> Result<ExecResponse, SessionError>;

    fn execution_state_dirty(&self) -> bool;

    fn snapshot_execution_state(
        &mut self,
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError>;

    fn probe_execution_state_capture(&mut self) -> Result<(), SessionError>;

    fn hydrated_execution_state(
        &self,
    ) -> Result<lash_core::plugin::HydratedExecutionState, SessionError>;

    fn acknowledge_execution_state_capture(&mut self) -> Result<(), SessionError>;

    fn abort_execution_state_capture(&mut self) -> Result<(), SessionError>;

    fn restore_execution_state(
        &mut self,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), SessionError>;

    fn prune_protected_globals(
        &mut self,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError>;

    fn patch_globals(
        &mut self,
        patch: &RlmGlobalsPatchPluginBody,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError>;

    fn prepare_bound_variables_prompt(
        &self,
        exclude: &BTreeSet<String>,
    ) -> Result<BoundVariablesPromptRender, SessionError>;
}

/// The dialect-specific words and call forms every shared prompt fragment
/// needs.
///
/// Prompt copy was dialect-aware only where it was obviously a *cell* — the
/// execution section, the retry copy, the finalization copy. Everything else
/// assembled around those (bound variables, read-only variables, tool docs,
/// budget escalation, the final-answer instruction) was written when Lashlang
/// was the only dialect and hardcoded its syntax. A TypeScript session was
/// therefore told, in the same prompt, to write `<typescript>` cells and that
/// its variables were "bound in lashlang ... in `<lashlang>` blocks". A model
/// cannot follow both; the judged battery caught one spending reasoning tokens
/// reconciling the contradiction.
///
/// One struct rather than a dozen trait methods, so a new fragment has an
/// obvious place to read its words from and `no_cross_dialect_text_in_the_
/// assembled_prompt` has one source of truth to check against.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DialectPromptVocabulary {
    /// How the prompt names the language in prose.
    pub(crate) language_name: &'static str,
    /// The opening cell tag, quoted in prose that points at cells.
    pub(crate) cell_open_tag: &'static str,
    /// What the prompt calls one unit of code: Lashlang says "block".
    pub(crate) cell_noun: &'static str,
    /// The call that prints a value for inspection.
    pub(crate) print_call: &'static str,
    /// `print x` vs `console.log(x)`, ready to take a value expression.
    pub(crate) print_statement_prefix: &'static str,
    pub(crate) print_statement_suffix: &'static str,
    /// The finish form as the prompt spells it in prose.
    pub(crate) finish_statement: &'static str,
    /// The continue-as control call, as a model would write it.
    pub(crate) continue_as_call: &'static str,
    /// A complete continue-as example for the tool doc.
    pub(crate) continue_as_example: &'static str,
}

impl Default for DialectPromptVocabulary {
    /// The default dialect's words, matching `RlmDialect::default()`.
    fn default() -> Self {
        crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY
    }
}

impl DialectPromptVocabulary {
    /// `print x` / `console.log(x)` for one expression.
    pub(crate) fn print_statement(&self, expression: &str) -> String {
        format!(
            "{}{expression}{}",
            self.print_statement_prefix, self.print_statement_suffix
        )
    }
}

pub(crate) trait RlmDialect: Send + Sync {
    fn language_id(&self) -> &'static str;

    /// The words shared prompt fragments use when they name this dialect's
    /// syntax. See [`DialectPromptVocabulary`].
    fn prompt_vocabulary(&self) -> DialectPromptVocabulary;

    /// The call path a model writes to invoke `tool` in this dialect.
    fn tool_call_path(&self, manifest: &lash_core::ToolManifest) -> Result<String, SessionError>;

    /// One authored tool example, in this dialect's syntax.
    ///
    /// Examples are authored once, as Lashlang source, next to the tool that
    /// owns them (`await web.search({ query: "..." })?`). They are a second
    /// model-facing surface on top of the call path, and the `?` try-operator
    /// that six of seven examples in the resident catalog carry is a *syntax
    /// error* in TypeScript: a judged row's saved prompt showed a TypeScript
    /// session being shown seven examples it could not have run.
    fn render_tool_example(&self, example: &str) -> String {
        example.to_string()
    }

    fn snapshot_engine_id(&self) -> &'static str;

    fn cell_tags(&self) -> CellTags;

    fn create_session(&self) -> Result<Box<dyn RlmDialectSession>, SessionError>;

    fn render_execution_section(
        &self,
        features: crate::protocol::RlmPromptFeatures,
        tool_catalog: &lash_core::ToolCatalog,
    ) -> Result<String, SessionError>;

    fn render_history_cell(&self, prose: &str, code: &str) -> String {
        crate::cell_scan::render_cell_text(self.cell_tags(), prose, code)
    }

    fn finalization_copy(&self, termination: &lash_rlm_types::RlmTermination) -> &'static str;

    fn cell_error_message(&self, error: crate::protocol::CellExtractionError) -> String;

    fn turn_limit_final_copy(&self, max_turns: usize) -> String;

    fn finish_required_copy(&self, requires_schema: bool) -> String;

    fn finish_schema_mismatch_copy(&self) -> String;

    fn invalid_cell_retry_copy(&self, error_text: &str) -> String;

    /// What to tell a model that wrote a cell in a registered dialect this
    /// session is not running.
    ///
    /// Written from the vocabulary, so the correction is in the reader's own
    /// words: naming the tag it wrote and the one it must write is the whole
    /// content, and both are facts the dialect already owns.
    fn foreign_cell_retry_copy(&self, foreign_open_tag: &str) -> String {
        let vocabulary = self.prompt_vocabulary();
        let tags = self.cell_tags();
        format!(
            "That reply put its code in a `{foreign_open_tag}` {noun}, which this session does not run. This session executes {language}: send the same work again inside one paired `{open}` … `{close}` {noun}.",
            noun = vocabulary.cell_noun,
            language = vocabulary.language_name,
            open = tags.open,
            close = tags.close,
        )
    }

    /// What to tell a model that opened a line with this dialect's tag in a
    /// position the cell grammar refuses.
    ///
    /// The rule itself is the whole content, because the failure this replaces
    /// was a reply that got no rule at all: a misplaced fence was read as prose,
    /// the driver answered "please finish", and the model — correctly seeing
    /// nothing wrong with its own code — re-sent it until the turn's budget died
    /// (FIG-1475). Written from the dialect's own tags and noun, so the
    /// correction is in the reader's words.
    ///
    /// It names the *canonical* shape only, and deliberately says nothing about
    /// the one-line shape the scanner also reads. Every prompt fragment teaches
    /// standalone tag lines; a correction that advertised a second accepted
    /// shape would contradict them, and this copy exists to remove a
    /// contradiction rather than add one. A reply already in the one-line shape
    /// never reaches this copy — it executes.
    fn malformed_cell_fence_retry_copy(&self) -> String {
        let vocabulary = self.prompt_vocabulary();
        let tags = self.cell_tags();
        format!(
            "That reply opened a line with `{open}` in a position the {noun} grammar could not read, so nothing ran and no code was executed. The tag lines are what this depends on: `{open}` must stand alone on its own line with nothing else on it, the source goes on the lines after it, and `{close}` must stand alone on a later line.",
            noun = vocabulary.cell_noun,
            open = tags.open,
            close = tags.close,
        )
    }

    fn output_limit_cell_copy(&self, output_token_cap: Option<usize>) -> String;

    fn code_stream_kind(&self) -> &'static str;

    fn execution_diagnostic_name(&self) -> &'static str;

    fn stream_cell_start_event_name(&self) -> &'static str;

    fn stream_cell_end_event_name(&self) -> &'static str;
}

/// The TypeScript dialect's words, for assertions that need a vocabulary which
/// is provably not the default.
#[cfg(test)]
pub(crate) fn typescript_prompt_vocabulary() -> DialectPromptVocabulary {
    typescript::TYPESCRIPT_PROMPT_VOCABULARY
}

#[derive(Clone)]
pub(crate) struct RlmDialectRegistry {
    dialects: Arc<BTreeMap<&'static str, Arc<dyn RlmDialect>>>,
}

impl RlmDialectRegistry {
    pub(crate) fn new(dialects: impl IntoIterator<Item = Arc<dyn RlmDialect>>) -> Self {
        let dialects = dialects
            .into_iter()
            .map(|dialect| (dialect.language_id(), dialect))
            .collect();
        Self {
            dialects: Arc::new(dialects),
        }
    }

    pub(crate) fn resolve(
        &self,
        language: &str,
    ) -> Result<Arc<dyn RlmDialect>, DialectRegistryError> {
        self.dialects
            .get(language)
            .cloned()
            .ok_or_else(|| DialectRegistryError::Unregistered {
                language: language.to_string(),
            })
    }

    pub(crate) fn resolve_active(
        &self,
        language: &str,
        active: &str,
    ) -> Result<Arc<dyn RlmDialect>, DialectRegistryError> {
        let dialect = self.resolve(language)?;
        if language != active {
            return Err(DialectRegistryError::Inactive {
                language: language.to_string(),
                active: active.to_string(),
            });
        }
        Ok(dialect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_resolves_registered_typescript_language() {
        let dialect: Arc<dyn RlmDialect> = Arc::new(TypescriptDialect::new(
            lash_lashlang_runtime::LashlangSurface::default(),
            LashlangDialectServices {
                projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
                artifact_store: ::lashlang::global_in_memory_lashlang_artifact_store(),
                deferred_tool_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
            },
        ));
        let registry = RlmDialectRegistry::new([dialect]);

        assert_eq!(
            registry
                .resolve("typescript")
                .expect("typescript is registered")
                .language_id(),
            "typescript"
        );
    }

    /// `RlmDialect::ALL` is what every host offers a dialect choice from, so it
    /// has to name exactly the dialects this registry can activate. Checked
    /// against the dialect implementations themselves rather than against a
    /// second list of names: a dialect the registry gains and the array lacks
    /// is a create form that cannot select it, and a name the array gains
    /// without a dialect is a create form that offers one the executor refuses.
    #[test]
    fn the_public_dialect_array_names_every_registered_dialect() {
        let registry = RlmDialectRegistry::new([
            Arc::new(lashlang_test_dialect()) as Arc<dyn RlmDialect>,
            Arc::new(typescript_test_dialect()) as Arc<dyn RlmDialect>,
        ]);

        let mut registered = registry.dialects.keys().copied().collect::<Vec<_>>();
        registered.sort_unstable();
        let mut published = lash_rlm_types::RlmDialect::ALL
            .iter()
            .map(|dialect| dialect.language_id())
            .collect::<Vec<_>>();
        published.sort_unstable();

        assert_eq!(published, registered);
        for language_id in registered {
            assert_eq!(
                lash_rlm_types::RlmDialect::from_language_id(language_id)
                    .expect("a registered language id resolves to a typed dialect")
                    .language_id(),
                language_id
            );
        }
        assert_eq!(
            lash_rlm_types::RlmDialect::from_language_id("lashscript"),
            None,
            "an unregistered language id must refuse rather than default"
        );
    }
}

#[cfg(test)]
pub(crate) fn test_dialect_services() -> LashlangDialectServices {
    LashlangDialectServices {
        projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
        artifact_store: ::lashlang::global_in_memory_lashlang_artifact_store(),
        deferred_tool_resolver: None,
        execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
        execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
    }
}

#[cfg(test)]
pub(crate) fn lashlang_test_dialect() -> LashlangDialect {
    LashlangDialect::new(
        lash_lashlang_runtime::LashlangSurface::default(),
        test_dialect_services(),
    )
}

#[cfg(test)]
pub(crate) fn typescript_test_dialect() -> TypescriptDialect {
    TypescriptDialect::new(
        lash_lashlang_runtime::LashlangSurface::default(),
        test_dialect_services(),
    )
}

#[cfg(test)]
#[path = "dialect/prompt_walker_tests.rs"]
mod prompt_walker_tests;
