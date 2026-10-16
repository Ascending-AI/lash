//! The replaceable tool presenter of an execution-mode plugin.
//!
//! An execution-mode plugin (code mode, the standard protocol) shows a model
//! the tools it may call. Which tools it offers, how a call is admitted and
//! validated, and what a call writes are the mode's and the contract's; how
//! the offered tools *read* is a [`ToolPresenter`]'s. The mode holds one in a
//! physical [`ToolPresenterSlot`], hands it the offered tools with their
//! contracts and the call paths it validated, and maps the text it returns
//! into the section the mode registers. A host replaces or wraps the
//! presenter; it cannot change what is offered or what a call accepts, since
//! every argument still passes the contract's canonical validation.
//!
//! A session records the presenter's id and the complete config
//! [`ToolPresenter::admit_config`] admitted at its creation
//! ([`RecordedToolPresenter`]). Every later open presents under that record,
//! never under the opening host's defaults, and a host whose slot holds
//! another presenter is refused.
//!
//! The shipped presenters live in their own crates; [`listing`] is the
//! catalogue listing they share and a host presenter can reuse, and
//! [`WrappedToolPresenter`] decorates any presenter in a few lines.

use std::sync::Arc;

use lash_sansio::{JsonSchema, ToolContract, ToolId, ToolManifest};
use serde_json::Value;

pub mod listing;
mod projection;
mod wrap;

pub use projection::{inline_schema, inline_shape};

pub use wrap::WrappedToolPresenter;

/// How a mode's tools read to a model.
pub trait ToolPresenter: Send + Sync {
    /// The presenter's identity: what a session records, and what a session
    /// reopened under another presenter is refused for.
    fn id(&self) -> &str;

    /// The complete config this presenter runs under, from a host's `raw`
    /// one: every parameter stated, so a session that records it never reads
    /// a default again.
    ///
    /// # Errors
    ///
    /// [`PresentationRefusal::Config`] for a config this presenter does not
    /// take.
    fn admit_config(&self, raw: &Value) -> Result<Value, PresentationRefusal>;

    /// The tools section for `request`'s tools under its admitted config:
    /// its text and which tools it lists.
    ///
    /// # Errors
    ///
    /// A refusal to present a tool or to read the config.
    fn present(
        &self,
        request: ToolPresentationRequest<'_>,
    ) -> Result<PresentedToolSection, PresentationRefusal>;

    /// One tool's doc, as the section would list it: what a discovery
    /// operation answers for a tool it found.
    ///
    /// # Errors
    ///
    /// A refusal to present the tool or to read the config.
    fn tool_doc(
        &self,
        tool: &ToolPresentationEntry<'_>,
        config: &Value,
    ) -> Result<String, PresentationRefusal>;

    /// The one-line signature of a tool called by `call_path` with these
    /// admitted schemas, without documentation.
    ///
    /// # Errors
    ///
    /// A refusal to read the config.
    fn tool_signature(
        &self,
        call_path: &str,
        input: &JsonSchema,
        output: &JsonSchema,
        config: &Value,
    ) -> Result<String, PresentationRefusal>;

    /// The text a host's discovery search matches a tool against.
    ///
    /// # Errors
    ///
    /// A refusal to read the config.
    fn discovery_text(
        &self,
        manifest: &ToolManifest,
        contract: &ToolContract,
        config: &Value,
    ) -> Result<String, PresentationRefusal>;
}

/// One offered tool, as its mode hands it to the presenter.
#[derive(Clone, Copy, Debug)]
pub struct ToolPresentationEntry<'a> {
    pub manifest: &'a ToolManifest,
    pub contract: &'a ToolContract,
    /// The path a call of the tool writes, as the mode validated it.
    pub call_path: &'a str,
    /// Listed whatever the budget: the discovery tool and the control tools.
    pub pinned: bool,
}

/// How much of the offered catalogue a section lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolDisclosure {
    /// Every offered tool.
    Complete,
    /// The pinned tools, then the tools whose docs fit `catalogue_bytes`;
    /// the rest are left to the mode's discovery operation.
    Budget { catalogue_bytes: usize },
}

/// What a mode asks its presenter to present.
#[derive(Clone, Copy, Debug)]
pub struct ToolPresentationRequest<'a> {
    /// The offered tools, in catalogue order.
    pub tools: &'a [ToolPresentationEntry<'a>],
    /// The complete config the session recorded.
    pub config: &'a Value,
    pub disclosure: ToolDisclosure,
}

/// A presented tools section.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PresentedToolSection {
    pub text: String,
    /// The listed tools, in catalogue order.
    pub listed: Vec<ToolId>,
    /// The tools left to discovery, in catalogue order.
    pub omitted: Vec<ToolId>,
}

impl PresentedToolSection {
    /// Whether this section accounts for exactly `request`'s tools: each
    /// listed or omitted once, and every pinned one listed. A mode refuses a
    /// section that does not.
    pub fn accounts_for(&self, request: &ToolPresentationRequest<'_>) -> bool {
        let mut answered = self.listed.iter().chain(&self.omitted).collect::<Vec<_>>();
        answered.sort();
        let mut offered = request
            .tools
            .iter()
            .map(|tool| &tool.manifest.id)
            .collect::<Vec<_>>();
        offered.sort();
        answered == offered
            && request
                .tools
                .iter()
                .filter(|tool| tool.pinned)
                .all(|tool| self.listed.contains(&tool.manifest.id))
    }
}

/// Why a presenter refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PresentationRefusal {
    /// The schema cannot be represented without losing contract information.
    #[error("tool schema cannot be presented: {cause}")]
    Shape {
        #[from]
        cause: lash_tool_shape::ShapeImportRefusal,
    },
    /// The presenter does not take this config.
    #[error("tool presenter `{presenter}` refuses its config: {reason}")]
    Config { presenter: String, reason: String },
    /// The presenter cannot present this tool.
    #[error("tool presenter `{presenter}` cannot present tool `{tool}`: {reason}")]
    Tool {
        presenter: String,
        tool: String,
        reason: String,
    },
    /// The section does not account for exactly the offered tools.
    #[error(
        "tool presenter `{presenter}` answered a listing that does not account for the offered tools"
    )]
    Listing { presenter: String },
}

impl PresentationRefusal {
    /// The refusal of `raw` as `presenter`'s config.
    pub fn config(presenter: &str, reason: impl std::fmt::Display) -> Self {
        Self::Config {
            presenter: presenter.to_owned(),
            reason: reason.to_string(),
        }
    }
}

/// The presenter a session records at creation: its id and the complete
/// config it admitted.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedToolPresenter {
    pub id: String,
    pub config: Value,
}

impl RecordedToolPresenter {
    /// What a session created with `presenter` under the host's `raw` config
    /// records.
    ///
    /// # Errors
    ///
    /// The presenter refuses `raw`.
    pub fn admit(presenter: &dyn ToolPresenter, raw: &Value) -> Result<Self, PresentationRefusal> {
        Ok(Self {
            id: presenter.id().to_owned(),
            config: presenter.admit_config(raw)?,
        })
    }
}

/// A mode's physical presenter slot: bound live, compared by id, never
/// stored.
#[derive(Clone)]
pub struct ToolPresenterSlot(pub Arc<dyn ToolPresenter>);

impl ToolPresenterSlot {
    pub fn new(presenter: impl ToolPresenter + 'static) -> Self {
        Self(Arc::new(presenter))
    }

    pub fn presenter(&self) -> &dyn ToolPresenter {
        self.0.as_ref()
    }

    /// Whether this slot holds the presenter `recorded` names.
    pub fn holds(&self, recorded: &RecordedToolPresenter) -> bool {
        self.0.id() == recorded.id
    }

    /// What a session created with this presenter under the host's `raw`
    /// config records.
    ///
    /// # Errors
    ///
    /// The presenter refuses `raw`.
    pub fn admit(&self, raw: &Value) -> Result<RecordedToolPresenter, PresentationRefusal> {
        RecordedToolPresenter::admit(self.presenter(), raw)
    }
}

impl std::fmt::Debug for ToolPresenterSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ToolPresenterSlot")
            .field(&self.0.id())
            .finish()
    }
}

impl PartialEq for ToolPresenterSlot {
    fn eq(&self, other: &Self) -> bool {
        self.0.id() == other.0.id()
    }
}

impl Eq for ToolPresenterSlot {}

/// Admits `raw` over `standard` as a presenter's complete config `C`: the
/// keys `raw` states replace the standard ones, and the result states every
/// key. `raw` must be an object.
///
/// ```rust,ignore
/// // A presenter whose config is `{ schema_depth }`, standard 8.
/// let admitted = admit_over_standard("host.tools.v1", &MyConfig::standard(), &json!({}))?;
/// assert_eq!(admitted, json!({ "schema_depth": 8 }));
/// ```
///
/// # Errors
///
/// [`PresentationRefusal::Config`] for a `raw` that is not an object or does
/// not read as `C`.
pub fn admit_over_standard<C>(
    presenter: &str,
    standard: &C,
    raw: &Value,
) -> Result<Value, PresentationRefusal>
where
    C: serde::Serialize + serde::de::DeserializeOwned,
{
    let Value::Object(stated) = raw else {
        return Err(PresentationRefusal::config(
            presenter,
            "the config is not an object",
        ));
    };
    let mut complete = serde_json::to_value(standard)
        .map_err(|error| PresentationRefusal::config(presenter, error))?;
    let Value::Object(fields) = &mut complete else {
        return Err(PresentationRefusal::config(
            presenter,
            "the standard config is not an object",
        ));
    };
    for (key, value) in stated {
        fields.insert(key.clone(), value.clone());
    }
    let admitted: C = read_config(presenter, &complete)?;
    serde_json::to_value(admitted).map_err(|error| PresentationRefusal::config(presenter, error))
}

/// Reads a complete admitted `config` as `C`.
///
/// ```rust,ignore
/// let config: MyConfig = read_config("host.tools.v1", request.config)?;
/// ```
///
/// # Errors
///
/// [`PresentationRefusal::Config`] when `config` does not read as `C`.
pub fn read_config<C: serde::de::DeserializeOwned>(
    presenter: &str,
    config: &Value,
) -> Result<C, PresentationRefusal> {
    C::deserialize(config).map_err(|error| PresentationRefusal::config(presenter, error))
}
