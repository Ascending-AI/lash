use super::*;
use std::sync::Mutex as StdMutex;

/// The task the frame switch hands the new frame.
pub const TASK: &str = "carry on from the summary";
/// A tool that switches the agent frame and hands the new frame [`TASK`].
pub struct SwitchFrame;

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for SwitchFrame {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::turn_control(lash_core::TurnControl::SwitchAgentFrame {
            next_run: call
                .context
                .run_spec()
                .expect("a tool body runs inside a turn")
                .next_run()
                .expect("the turn's spec encodes"),
            frame_key: lash_core::FrameKey::from_caller_material("facade-frame-switch")
                .expect("non-empty caller material"),
            initial_nodes: Vec::new(),
            task: TASK.to_owned(),
        })
        .into()
    }
}

/// The tool an account add brings: the workbench's mail tools, in small.
pub const ACCOUNT_TOOL: &str = "inbox_work";

/// A tool source whose tools are the accounts added so far.
pub struct AccountTools {
    pub names: Arc<StdMutex<Vec<String>>>,
}

pub fn account_tool(name: &str) -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{name}"),
        format!("tools.{name}"),
        "Reads an account's inbox.",
        lash_vocabulary::tool_contract::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
    .expect("the account tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

#[async_trait]
impl ToolProvider for AccountTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash::plugins::PluginError,
    > {
        Ok({
            self.names
                .lock_recover()
                .iter()
                .map(|name| account_tool(name).manifest())
                .collect()
        })
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        self.names
            .lock_recover()
            .iter()
            .find(|tool| format!("tools.{tool}") == name)
            .map(|tool| Arc::new(account_tool(tool).contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!("empty")).into()
    }
}

/// The tools each model request offered, by the request's input text.
pub type OfferedTools = Arc<StdMutex<Vec<(String, Vec<String>)>>>;

/// A core over SQLite memory stores whose model echoes each input and
/// records the tools each request offered, by the input's text. A turn
/// whose input is `hold` waits, once it reached the model, for `release`.
pub struct AccountCore {
    pub core: LashCore,
    pub accounts: Arc<StdMutex<Vec<String>>>,
    pub offered: OfferedTools,
    pub entered: Arc<tokio::sync::Semaphore>,
    pub release: Arc<tokio::sync::Semaphore>,
}

impl AccountCore {
    pub async fn new() -> Self {
        let accounts = Arc::new(StdMutex::new(Vec::new()));
        let offered: OfferedTools = Arc::new(StdMutex::new(Vec::new()));
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let provider = lash_core::testing::TestProvider::builder()
            .kind("account-tools")
            .complete({
                let offered = Arc::clone(&offered);
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                move |request: LlmRequest| {
                    let text = last_user_text(&request);
                    let tools = request.tools.iter().map(|tool| tool.name.clone()).collect();
                    offered.lock_recover().push((text.clone(), tools));
                    let (entered, release) = (Arc::clone(&entered), Arc::clone(&release));
                    async move {
                        if text == "hold" {
                            entered.add_permits(1);
                            release.acquire().await.expect("released").forget();
                        }
                        Ok(text_response(&format!("echo: {text}")))
                    }
                }
            })
            .build()
            .into_handle();
        let core = explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(
            lash::tool_provider_key!("tools"),
            Arc::new(AccountTools {
                names: Arc::clone(&accounts),
            }),
        )
        .build(runtime_lease_owner())
        .expect("standard core");
        Self {
            core,
            accounts,
            offered,
            entered,
            release,
        }
    }

    /// The tools the request for input `text` offered.
    pub fn offered_to(&self, text: &str) -> Vec<String> {
        self.offered
            .lock_recover()
            .iter()
            .find(|(input, _)| input == text)
            .map(|(_, tools)| tools.clone())
            .unwrap_or_else(|| panic!("no request for {text:?}"))
    }
}

/// Add an account as the workbench does: the account's tool appears at its
/// source, and a tool-catalog refresh is queued on the session.
pub async fn add_account(
    accounts: &AccountCore,
    admin: &lash::LashSession,
    key: &str,
) -> lash_core::facade_support::SessionCommandReceipt {
    accounts
        .accounts
        .lock_recover()
        .push(ACCOUNT_TOOL.to_owned());
    admin
        .admin()
        .commands()
        .refresh_tool_catalog("account_added", key)
        .await
        .expect("the refresh is queued")
}

pub fn assert_settled(settlement: &lash_core::runtime::SessionCommandSettlement) {
    assert!(
        matches!(
            settlement,
            lash_core::runtime::SessionCommandSettlement::Durable(_)
        ),
        "the refresh settles: {settlement:?}"
    );
}

/// A host's prompt plugin: it replaces the standard protocol's intro with a
/// wrapper and adds a late section of its own (ADR 0133).
pub struct HostSections;

impl PluginFactory for HostSections {
    fn id(&self) -> &'static str {
        "host_sections"
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> {
        let mut reg = lash::plugins::PluginRegistrar::<HostSections>::new(
            lash::plugins::PluginFactory::id(self.as_ref()),
        );
        HostSections::declare(&mut reg)?;
        let id = self.id();
        Ok(lash::plugins::PluginRecipe::new(
            id,
            reg,
            move |_ctx, _state| Ok(Arc::new(HostSections)),
        ))
    }
}

impl HostSections {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("host_sections")
    }
}
impl lash_core::plugin::PluginMetadata for HostSections {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

impl lash_core::plugin::SessionPlugin for HostSections {
    fn id(&self) -> &'static str {
        "host_sections"
    }
}
impl HostSections {
    pub fn declare(
        reg: &mut lash::plugins::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash::plugins::PluginError> {
        reg.prompt().section(
            lash::plugins::PromptSectionSpec::new(
                lash::prompt::PromptSectionKey::new("intro").expect("valid section key"),
                lash::prompt::PromptPlacement::InitialInstructions,
            ),
            move |_this, _state| {
                Ok(Arc::new(|_: &lash::plugins::PromptInput<'_>| {
                    Ok(lash::plugins::SectionText::text(
                        "You are the release desk.",
                    ))
                }))
            },
        )?;

        reg.prompt().section(
            lash::plugins::PromptSectionSpec::new(
                lash::prompt::PromptSectionKey::new("release").expect("valid section key"),
                lash::prompt::PromptPlacement::CurrentContext,
            ),
            move |_this, _state| {
                Ok(Arc::new(|_: &lash::plugins::PromptInput<'_>| {
                    Ok(lash::plugins::SectionText::text(
                        "Release 4.2 freezes on Friday.",
                    ))
                }))
            },
        )?;
        Ok(())
    }
}

/// A host plugin whose one section refuses to render.
pub struct BrokenSection;

impl PluginFactory for BrokenSection {
    fn id(&self) -> &'static str {
        "broken_section"
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> {
        let mut reg = lash::plugins::PluginRegistrar::<BrokenSection>::new(
            lash::plugins::PluginFactory::id(self.as_ref()),
        );
        BrokenSection::declare(&mut reg)?;
        let id = self.id();
        Ok(lash::plugins::PluginRecipe::new(
            id,
            reg,
            move |_ctx, _state| Ok(Arc::new(BrokenSection)),
        ))
    }
}

impl BrokenSection {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("broken_section")
    }
}
impl lash_core::plugin::PluginMetadata for BrokenSection {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

impl lash_core::plugin::SessionPlugin for BrokenSection {
    fn id(&self) -> &'static str {
        "broken_section"
    }
}
impl BrokenSection {
    pub fn declare(
        reg: &mut lash::plugins::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash::plugins::PluginError> {
        reg.prompt().section(
            lash::plugins::PromptSectionSpec::new(
                lash::prompt::PromptSectionKey::new("calendar").expect("valid section key"),
                lash::prompt::PromptPlacement::CurrentContext,
            ),
            move |_this, _state| {
                Ok(Arc::new(|_: &lash::plugins::PromptInput<'_>| {
                    Err::<lash::plugins::SectionText, _>(lash::plugins::PromptRenderError::new(
                        "the release calendar is unreachable",
                    ))
                }))
            },
        )?;
        Ok(())
    }
}
