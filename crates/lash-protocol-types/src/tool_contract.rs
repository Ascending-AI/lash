use crate::{
    JsonSchema, SchemaContract, SchemaDialect, SchemaProjectionOverride, SchemaPurpose,
    ToolCatalogBuildError, ToolDeclaration,
};
pub use lash_kernel_dialect::QualifiedOperation;

/// The contract for executing one logical call, pinned before its first attempt.
///
/// Recovery never re-executes an effect whose outcome committed. These rules
/// require acknowledged database commits to survive the failure; exactly-once
/// external behaviour requires host deduplication on `call_id`. See
/// [What survives a crash](https://github.com/Ascending-AI/lash/blob/main/README.md#what-survives-a-crash).
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionPolicy {
    /// One application attempt, even after a reported pre-effect failure.
    /// A crash after its start commits but before its outcome commits settles
    /// `Interrupted` without re-entering the body. Its external action may have
    /// happened; this policy does not promise successful completion.
    #[default]
    Once,
    /// Repeated execution is part of the tool's declared contract.
    /// An interrupted attempt may re-enter its same ordinal; reported failures
    /// retry within the recorded bounds and current declaration. `max_attempts`
    /// counts admitted ordinals, not body entries across repeated crashes.
    Repeatable { retry: BoundedRetry },
    /// A read: the answer is not recorded; after a crash before the next
    /// save it is fetched again and may differ.
    ///
    /// A code cell runs the call inside the statement that performs it,
    /// through the provider's registered reader, and keeps the answer only
    /// as part of the cell's saved state. No admission, attempt or outcome
    /// is recorded for it, and its provider stays owed until the cell ends.
    /// A call that is recorded anyway (a model round's, or one a cell
    /// cannot plan) runs again from its start when its attempt was
    /// interrupted, and is never retried after a reported failure. A read
    /// changes nothing outside: it defers, isolates, declares intents and
    /// ends turns never.
    Read,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BoundedRetry {
    /// Counts admitted application ordinals, including the first attempt.
    pub max_attempts: std::num::NonZeroU32,
    pub backoff: Backoff,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Backoff {
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LimitCause {
    ExecutionSlice,
    ExecutionTotal,
    WaitDeadline,
}

impl ExecutionPolicy {
    pub fn repeatable(
        max_attempts: std::num::NonZeroU32,
        base_delay_ms: u64,
        max_delay_ms: u64,
    ) -> Self {
        Self::Repeatable {
            retry: BoundedRetry {
                max_attempts,
                backoff: Backoff {
                    base_delay_ms,
                    max_delay_ms,
                },
            },
        }
    }

    /// The bounded retry a `Repeatable` call is admitted with; `None` for a
    /// policy that never retries a reported failure.
    pub fn retry(self) -> Option<BoundedRetry> {
        match self {
            Self::Repeatable { retry } => Some(retry),
            Self::Once | Self::Read => None,
        }
    }

    /// A current declaration may veto a repeat; it cannot upgrade admission.
    pub fn permits_repeat(self, current: Self, failed_ordinal: u32) -> bool {
        matches!((self, current), (Self::Repeatable { retry }, Self::Repeatable { .. }) if failed_ordinal < retry.max_attempts.get())
    }

    /// Whether an admitted attempt that started and never settled runs
    /// again at its ordinal, rather than settling `Interrupted`: a
    /// `Repeatable` call, whose contract permits repeats, and a recorded
    /// `Read`, which changes nothing outside.
    pub fn reruns_interrupted(self) -> bool {
        matches!(self, Self::Repeatable { .. } | Self::Read)
    }
}

impl BoundedRetry {
    /// The delay before retry `retry_index` (0 for the first retry): the
    /// failure's suggested delay, or the doubling backoff from its base,
    /// capped at its maximum.
    pub fn delay_ms(self, retry_index: u32, suggested_delay_ms: Option<u64>) -> u64 {
        let multiplier = 1_u64.checked_shl(retry_index).unwrap_or(u64::MAX);
        suggested_delay_ms
            .unwrap_or_else(|| self.backoff.base_delay_ms.saturating_mul(multiplier))
            .min(self.backoff.max_delay_ms)
    }
}

/// How long a deferring tool's call may stay parked, waiting for its
/// completion: its host's choice, declared beside its
/// [`ToolManifest::execution`] and never inside the closed
/// [`ToolDeclaration`]. The deadline it sets is computed once, when the call
/// is admitted, and pinned with the call's run: a crash or takeover never
/// refreshes it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ParkBound {
    /// The park ends `TimedOut` this long after the call's admission.
    Within(std::time::Duration),
    /// The park has no deadline: it lasts until it resolves or the scope
    /// that owns the call (its turn or its process) ends, which revokes it.
    UntilScopeEnd,
}

/// The bound a tool's manifest must declare, named by
/// [`RegistrationRefused::MissingBound`].
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolBound {
    /// [`ToolManifest::execution`], which every tool declares.
    Execution,
    /// [`ToolManifest::park`], which every tool that may defer declares.
    Park,
}

impl std::fmt::Display for ToolBound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Execution => "execution",
            Self::Park => "park",
        })
    }
}

/// A tool's bounds, as its manifest holds them ([`ToolManifest::bounds`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolBounds {
    /// How long one run of the tool's body may take.
    pub execution: std::time::Duration,
    /// How long a call may stay parked; `None` for a tool that never defers.
    pub park: Option<ParkBound>,
}

/// Why registration refused a tool: why its manifest cannot be built, or why
/// a catalog does not take it.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum RegistrationRefused {
    /// The tool's manifest does not declare a bound its host must set: its
    /// `execution`, or the `park` of a tool that may defer.
    #[error("tool `{tool}` declares no {bound} bound; its host must set one")]
    MissingBound { tool: String, bound: ToolBound },
    /// A tool that never defers declares a `park` bound it can never use.
    #[error("tool `{tool}` declares a park bound, but it does not declare `may_defer`")]
    ParkWithoutDeferral { tool: String },
    /// The tool's declaration is itself invalid.
    #[error("tool `{tool}`'s declaration is refused: {cause}")]
    Declaration {
        tool: String,
        cause: crate::DeclarationRefusal,
    },
    /// The tool is declared isolated and names no process engine to run in.
    #[error("tool `{tool}` is declared isolated, but it names no process engine")]
    IsolatedWithoutEngine { tool: String },
    /// A tool that is not isolated names a process engine it never runs in.
    #[error("tool `{tool}` names a process engine, but it does not declare `isolated`")]
    EngineWithoutIsolation { tool: String },
    /// The tool is isolated in a process engine its deployment does not
    /// register: an isolated call never falls back to an inline body.
    #[error("tool `{tool}` is isolated in process engine `{engine}`, which is not registered")]
    UnregisteredIsolationEngine { tool: String, engine: String },
    /// A tool defined with an output declares a turn control. A control
    /// call has no output, so its contract states none: the control is its
    /// result. [`ToolDefinition::control`] defines one.
    #[error("tool `{tool}` declares a turn control, but it is defined with an output")]
    ControlDeclaresOutput { tool: String },
    /// A `Read` tool declares a capability a read never has: deferral,
    /// intents, isolation or a turn control.
    #[error("tool `{tool}` is declared a read, and a read cannot {capability}")]
    ReadDeclares { tool: String, capability: String },
    /// A `Read` tool's provider registered no reader: a read never falls
    /// back to the provider's recorded execution.
    #[error("tool `{tool}` is declared a read, and its provider `{provider}` registers no reader")]
    ReadWithoutReader { tool: String, provider: String },
}

fn default_tool_execution_policy() -> ExecutionPolicy {
    ExecutionPolicy::default()
}

fn is_default_tool_execution_policy(policy: &ExecutionPolicy) -> bool {
    *policy == ExecutionPolicy::default()
}

#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOutputContract {
    #[default]
    Static,
    FromInputSchema {
        input_field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_schema: Option<JsonSchema>,
    },
}

impl ToolOutputContract {
    pub fn from_input_schema(
        input_field: impl Into<String>,
        default_schema: Option<JsonSchema>,
    ) -> Self {
        Self::FromInputSchema {
            input_field: input_field.into(),
            default_schema,
        }
    }

    pub fn is_static(&self) -> bool {
        matches!(self, Self::Static)
    }
}

#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolArgumentProjectionPolicy {
    #[default]
    MaterializeProjectedValues,
    PreserveProjectedRefsInField {
        field: String,
    },
}

impl ToolArgumentProjectionPolicy {
    pub fn preserve_projected_refs_in_field(field: impl Into<String>) -> Self {
        Self::PreserveProjectedRefsInField {
            field: field.into(),
        }
    }

    pub fn is_materialize_projected_values(&self) -> bool {
        matches!(self, Self::MaterializeProjectedValues)
    }
}

fn is_default_tool_argument_projection_policy(policy: &ToolArgumentProjectionPolicy) -> bool {
    policy.is_materialize_projected_values()
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ToolId(String);

/// The wire shape is the non-empty string; `ToolId::new`'s emptiness refusal
/// cannot be expressed as a schema assertion.
impl schemars::JsonSchema for ToolId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        <String as schemars::JsonSchema>::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <String as schemars::JsonSchema>::json_schema(generator)
    }
}

impl ToolId {
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        assert!(!id.trim().is_empty(), "tool id must not be empty");
        Self(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for ToolId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let id = <String as serde::Deserialize>::deserialize(deserializer)?;
        if id.trim().is_empty() {
            return Err(serde::de::Error::custom("tool id must not be empty"));
        }
        Ok(Self(id))
    }
}

impl From<String> for ToolId {
    fn from(id: String) -> Self {
        Self::new(id)
    }
}

impl From<&str> for ToolId {
    fn from(id: &str) -> Self {
        Self::new(id)
    }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Host-owned discovery operation used to find tools omitted from the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDiscovery {
    pub operation: String,
}

fn inline_default() -> bool {
    true
}
fn is_inline(value: &bool) -> bool {
    *value
}

/// Tool metadata exposed to prompts, catalogs, and UI. Catalog membership —
/// being present in a [`ToolProvider`]'s manifest list — is the execution gate;
/// there is no per-manifest tier. Its schemas stay in [`ToolContract`]; how
/// they read to a model is the execution mode's tool presenter's.
///
/// A manifest is complete by construction: it holds its host-set bounds and a
/// valid declaration, and one built or decoded without them is refused
/// ([`RegistrationRefused`]). Nothing that reads a manifest checks them again.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct ToolManifest {
    #[serde(default = "inline_default", skip_serializing_if = "is_inline")]
    pub inline: bool,
    pub id: ToolId,
    /// The operation a document performs the tool as, and the tool's key in
    /// a catalog and a recorded surface. Its model-facing name is derived
    /// ([`Self::standard_name`]).
    #[schemars(with = "String")]
    #[serde(with = "lash_kernel_dialect::operation_text")]
    pub operation: QualifiedOperation,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub bindings: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(
        default,
        skip_serializing_if = "is_default_tool_argument_projection_policy"
    )]
    pub argument_projection: ToolArgumentProjectionPolicy,
    #[serde(
        default = "default_tool_execution_policy",
        skip_serializing_if = "is_default_tool_execution_policy"
    )]
    pub execution_policy: ExecutionPolicy,
    /// How long one run of the tool's body may take, set by its host.
    execution: std::time::Duration,
    /// How long a call may stay parked waiting for its completion, set by
    /// its host. Held exactly by a tool whose declaration may defer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    park: Option<ParkBound>,
    /// The author's three-capability declaration. Admission records it with
    /// this manifest; dispatch, recovery and replay read the recorded answer,
    /// never the live provider.
    #[serde(default, skip_serializing_if = "ToolDeclaration::is_default")]
    declaration: ToolDeclaration,
    /// [`ProcessEngine::kind`] of the engine an isolated tool's calls run in.
    /// Held exactly by a tool whose declaration is isolated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    isolation_engine: Option<String>,
}

/// A manifest as it is stored, before its bounds and declaration are checked.
#[derive(serde::Deserialize)]
struct ManifestRecord {
    #[serde(default = "inline_default")]
    inline: bool,
    id: ToolId,
    #[serde(with = "lash_kernel_dialect::operation_text")]
    operation: QualifiedOperation,
    #[serde(default)]
    description: String,
    #[serde(default)]
    bindings: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    argument_projection: ToolArgumentProjectionPolicy,
    #[serde(default = "default_tool_execution_policy")]
    execution_policy: ExecutionPolicy,
    #[serde(default)]
    execution: Option<std::time::Duration>,
    #[serde(default)]
    park: Option<ParkBound>,
    #[serde(default)]
    declaration: ToolDeclaration,
    #[serde(default)]
    isolation_engine: Option<String>,
}

impl<'de> serde::Deserialize<'de> for ToolManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let record = ManifestRecord::deserialize(deserializer)?;
        let missing_execution = || RegistrationRefused::MissingBound {
            tool: record.operation.effect_name().to_string(),
            bound: ToolBound::Execution,
        };
        let execution = record
            .execution
            .ok_or_else(missing_execution)
            .map_err(serde::de::Error::custom)?;
        Self {
            inline: record.inline,
            id: record.id,
            operation: record.operation,
            description: record.description,
            bindings: record.bindings,
            argument_projection: record.argument_projection,
            execution_policy: record.execution_policy,
            execution,
            park: None,
            declaration: ToolDeclaration::default(),
            isolation_engine: None,
        }
        .declared(record.declaration, record.park, record.isolation_engine)
        .map_err(serde::de::Error::custom)
    }
}

/// The longest model-facing tool name.
const STANDARD_NAME_BUDGET: usize = 64;

impl ToolManifest {
    /// The tool's model-facing name in Standard mode: its operation's
    /// segments joined with `__` (`inbox__send_item`). A canonical segment
    /// holds no `__`, so distinct operations get distinct names. A name over
    /// the budget is truncated and ends in `_h` and eight base32 characters
    /// of the effect name's digest.
    #[must_use]
    pub fn standard_name(&self) -> String {
        standard_name(&self.operation)
    }

    /// The tool as a refusal or a log names it: its effect name.
    #[must_use]
    pub fn tool(&self) -> String {
        self.operation.effect_name().to_string()
    }

    /// How long one run of the tool's body may take.
    #[must_use]
    pub fn execution(&self) -> std::time::Duration {
        self.execution
    }

    /// How long a call may stay parked; `None` for a tool that never defers.
    #[must_use]
    pub fn park(&self) -> Option<ParkBound> {
        self.park
    }

    /// The tool's declaration.
    #[must_use]
    pub fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    /// The process engine an isolated tool's calls run in; `None` for a tool
    /// that is not isolated.
    #[must_use]
    pub fn isolation_engine(&self) -> Option<&str> {
        self.isolation_engine.as_deref()
    }

    /// The bounds this manifest holds.
    #[must_use]
    pub fn bounds(&self) -> ToolBounds {
        ToolBounds {
            execution: self.execution,
            park: self.park,
        }
    }

    /// This manifest with another execution bound.
    #[must_use]
    pub fn with_execution(mut self, execution: std::time::Duration) -> Self {
        self.execution = execution;
        self
    }

    /// This manifest declaring `declaration`, with what its host sets beside
    /// it: the `park` bound of a tool that may defer, and the process
    /// `isolation_engine` of an isolated one.
    ///
    /// # Errors
    ///
    /// An invalid declaration; a deferring tool without a park bound, or a
    /// park bound on a tool that never defers; an isolated tool without an
    /// engine, or an engine on a tool that is not isolated.
    pub fn declared(
        mut self,
        declaration: ToolDeclaration,
        park: Option<ParkBound>,
        isolation_engine: Option<String>,
    ) -> Result<Self, RegistrationRefused> {
        let tool = || self.tool();
        declaration
            .validate()
            .map_err(|cause| RegistrationRefused::Declaration {
                tool: tool(),
                cause,
            })?;
        if let Some(refused) = read_refusal(&self.tool(), self.execution_policy, &declaration) {
            return Err(refused);
        }
        match (declaration.may_defer, park) {
            (true, None) => {
                return Err(RegistrationRefused::MissingBound {
                    tool: tool(),
                    bound: ToolBound::Park,
                });
            }
            (false, Some(_)) => {
                return Err(RegistrationRefused::ParkWithoutDeferral { tool: tool() });
            }
            (true, Some(_)) | (false, None) => {}
        }
        match (declaration.isolated, &isolation_engine) {
            (true, None) => {
                return Err(RegistrationRefused::IsolatedWithoutEngine { tool: tool() });
            }
            (false, Some(_)) => {
                return Err(RegistrationRefused::EngineWithoutIsolation { tool: tool() });
            }
            (true, Some(_)) | (false, None) => {}
        }
        self.declaration = declaration;
        self.park = park;
        self.isolation_engine = isolation_engine;
        Ok(self)
    }
}

impl ToolManifest {
    /// Why this manifest is not a valid read: a `Read` policy beside a
    /// capability a read never has. `None` for a valid read and for every
    /// other policy. A surface refuses a tool this names.
    #[must_use]
    pub fn read_refusal(&self) -> Option<RegistrationRefused> {
        read_refusal(&self.tool(), self.execution_policy, &self.declaration)
    }
}

/// The Standard-mode name of `operation` ([`ToolManifest::standard_name`]).
#[must_use]
pub fn standard_name(operation: &QualifiedOperation) -> String {
    let joined = operation
        .package
        .iter()
        .chain(std::iter::once(&operation.name))
        .map(|segment| segment.as_str())
        .collect::<Vec<_>>()
        .join("__");
    if joined.len() <= STANDARD_NAME_BUDGET {
        return joined;
    }
    let suffix = format!("_h{}", digest_tag(operation.effect_name().as_str()));
    let mut head = joined[..STANDARD_NAME_BUDGET - suffix.len()].to_owned();
    while head.ends_with('_') {
        head.pop();
    }
    head + &suffix
}

/// Eight lower-case base32 characters of the digest of `text`: the suffix a
/// truncated or disambiguated name ends in.
#[must_use]
pub fn digest_tag(text: &str) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let digest = blake3::hash(text.as_bytes());
    let bytes = digest.as_bytes();
    let bits = u64::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], 0, 0, 0]);
    (0..8)
        .map(|index| char::from(ALPHABET[((bits >> (59 - 5 * index)) & 31) as usize]))
        .collect()
}

/// The refusal of `declaration` under a `Read` `policy`.
fn read_refusal(
    tool: &str,
    policy: ExecutionPolicy,
    declaration: &ToolDeclaration,
) -> Option<RegistrationRefused> {
    if policy != ExecutionPolicy::Read {
        return None;
    }
    let capability = if declaration.may_defer {
        "defer"
    } else if !declaration.intents.is_empty() {
        "declare intents"
    } else if declaration.isolated {
        "run isolated"
    } else if !declaration.controls.is_empty() {
        "end a turn"
    } else {
        return None;
    };
    Some(RegistrationRefused::ReadDeclares {
        tool: tool.to_owned(),
        capability: capability.to_owned(),
    })
}

/// Heavy tool contract resolved only when a prompt or call needs its schemas.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ToolContract {
    #[serde(skip)]
    identity: Option<ToolContractIdentity>,
    #[serde(default = "ToolContract::default_input_schema_contract")]
    pub input_schema: SchemaContract,
    #[serde(default)]
    pub output_schema: SchemaContract,
    #[serde(default, skip_serializing_if = "ToolOutputContract::is_static")]
    pub output_contract: ToolOutputContract,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolContractIdentity {
    id: ToolId,
    operation: QualifiedOperation,
}

impl Default for ToolContract {
    fn default() -> Self {
        Self {
            identity: None,
            input_schema: Self::default_input_schema_contract(),
            output_schema: JsonSchema::any().into(),
            output_contract: ToolOutputContract::Static,
        }
    }
}

impl ToolContract {
    /// Whether the contract states no output: the null schema, projected as
    /// itself, with a static output contract. The contract of every tool
    /// that declares a turn control, whose result is its control alone.
    pub fn states_no_output(&self) -> bool {
        self.output_schema.canonical() == &serde_json::json!({ "type": "null" })
            && self.output_schema.projection.is_default()
            && self.output_contract.is_static()
    }

    #[expect(
        clippy::expect_used,
        reason = "the default input schema is a fixed object schema"
    )]
    fn default_input_schema_contract() -> SchemaContract {
        SchemaContract::admit(Self::default_input_schema()).expect("valid default input schema")
    }

    pub fn default_input_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": true
        })
    }

    /// Whether this contract was resolved from the exact manifest identity.
    ///
    /// Contracts produced through [`ToolDefinition::contract`] carry this
    /// process-local identity. Standalone/deserialized contracts do not and
    /// must use a provider's authoritative by-id resolution path instead.
    pub fn matches_manifest_identity(&self, manifest: &ToolManifest) -> bool {
        self.identity.as_ref().is_some_and(|identity| {
            identity.id == manifest.id && identity.operation == manifest.operation
        })
    }

    pub fn model_tool(&self, manifest: &ToolManifest) -> ModelTool {
        ModelTool {
            name: manifest.standard_name(),
            description: manifest.description.clone(),
            input_schema: self.input_schema.clone(),
            output_schema: self.output_schema.clone(),
        }
    }
}

/// Static authoring helper for tools.
///
/// Composes the runtime [`ToolManifest`] and [`ToolContract`] projections. They
/// serialize under the explicit `manifest` and `contract` keys: the definition
/// is reachable from the persisted code mode execution-state envelope, whose
/// canonical-encoding invariant bans `#[serde(flatten)]` because a flattened
/// subtree has no declaration order a structural pre-pass can validate.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    pub manifest: ToolManifest,
    pub contract: ToolContract,
}

/// A tool as its author defines it, before its host sets its execution
/// bound. Lash supplies no bound: [`ToolDraft::with_execution`] makes the
/// [`ToolDefinition`] a provider registers, and nothing else does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDraft {
    id: ToolId,
    operation: QualifiedOperation,
    description: String,
    bindings: std::collections::BTreeMap<String, serde_json::Value>,
    contract: ToolContract,
    /// The turn controls a [`ToolDefinition::control`] draft declares.
    controls: crate::TurnControls,
}

impl ToolDraft {
    /// Tool identity.
    pub fn id(&self) -> &ToolId {
        &self.id
    }

    /// The operation a document performs the tool as.
    pub fn operation(&self) -> &QualifiedOperation {
        &self.operation
    }

    /// The tool's canonical schema contract.
    pub fn contract(&self) -> &ToolContract {
        &self.contract
    }

    /// The tool as a model is offered it.
    pub fn model_tool(&self) -> ModelTool {
        ModelTool {
            name: standard_name(&self.operation),
            description: self.description.clone(),
            input_schema: self.contract.input_schema.clone(),
            output_schema: self.contract.output_schema.clone(),
        }
    }

    /// Sets how long one run of the tool's body may take: the bound every
    /// tool's host sets, which makes the draft a definition.
    pub fn with_execution(self, execution: std::time::Duration) -> ToolDefinition {
        ToolDefinition {
            manifest: ToolManifest {
                inline: true,
                id: self.id,
                operation: self.operation,
                description: self.description,
                bindings: self.bindings,
                argument_projection: ToolArgumentProjectionPolicy::default(),
                execution_policy: default_tool_execution_policy(),
                execution,
                park: None,
                declaration: ToolDeclaration::default().with_controls(self.controls),
                isolation_engine: None,
            },
            contract: self.contract,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTool {
    pub name: String,
    pub description: String,
    pub input_schema: SchemaContract,
    pub output_schema: SchemaContract,
}

/// What names a tool's operation: a [`QualifiedOperation`], or its effect
/// name (`inbox.send_item`), checked canonical where it is given.
pub trait IntoOperation {
    /// # Errors
    ///
    /// Why the text names no canonical operation.
    fn into_operation(self) -> Result<QualifiedOperation, String>;
}

impl IntoOperation for QualifiedOperation {
    fn into_operation(self) -> Result<QualifiedOperation, String> {
        Ok(self)
    }
}

impl IntoOperation for &QualifiedOperation {
    fn into_operation(self) -> Result<QualifiedOperation, String> {
        Ok(self.clone())
    }
}

impl IntoOperation for &str {
    fn into_operation(self) -> Result<QualifiedOperation, String> {
        lash_kernel_dialect::operation_text::parse(self)
    }
}

impl IntoOperation for String {
    fn into_operation(self) -> Result<QualifiedOperation, String> {
        self.as_str().into_operation()
    }
}

impl IntoOperation for &String {
    fn into_operation(self) -> Result<QualifiedOperation, String> {
        self.as_str().into_operation()
    }
}

impl ToolDefinition {
    /// A tool performed as `operation`, taking `input_schema` and giving
    /// `output_schema`.
    ///
    /// # Errors
    ///
    /// [`ToolCatalogBuildError`]: an operation that is not canonical, or a
    /// schema that is not admitted.
    pub fn raw(
        id: impl Into<ToolId>,
        operation: impl IntoOperation,
        description: impl Into<String>,
        input_schema: serde_json::Value,
        output_schema: serde_json::Value,
    ) -> Result<ToolDraft, ToolCatalogBuildError> {
        let id = id.into();
        let operation = operation.into_operation().map_err(|message| {
            ToolCatalogBuildError::InvalidOperation {
                tool_id: id.clone(),
                message,
            }
        })?;
        let admit = |schema, purpose| {
            SchemaContract::admit(schema).map_err(|source| ToolCatalogBuildError::UnusableSchema {
                tool_id: id.clone(),
                operation: operation.effect_name().to_string(),
                purpose,
                source,
            })
        };
        let input_schema = admit(input_schema, SchemaPurpose::ToolInput)?;
        let output_schema = admit(output_schema, SchemaPurpose::ToolOutput)?;
        let description = description.into();
        Ok(ToolDraft {
            description,
            controls: crate::TurnControls::none(),
            bindings: std::collections::BTreeMap::new(),
            contract: ToolContract {
                identity: Some(ToolContractIdentity {
                    id: id.clone(),
                    operation: operation.clone(),
                }),
                input_schema,
                output_schema,
                ..ToolContract::default()
            },
            id,
            operation,
        })
    }

    /// A tool whose call may end its caller's turn with one of `controls`.
    /// It has no output: its result is the control alone, so its output
    /// schema is the null schema and no catalog renders a return for it.
    ///
    /// # Errors
    ///
    /// [`ToolCatalogBuildError`] for an input schema that is not admitted.
    pub fn control(
        id: impl Into<ToolId>,
        operation: impl IntoOperation,
        description: impl Into<String>,
        input_schema: serde_json::Value,
        controls: crate::TurnControls,
    ) -> Result<ToolDraft, ToolCatalogBuildError> {
        let mut draft = Self::raw(
            id,
            operation,
            description,
            input_schema,
            serde_json::json!({ "type": "null" }),
        )?;
        draft.controls = controls;
        Ok(draft)
    }

    pub fn typed<Args, Output>(
        id: impl Into<ToolId>,
        operation: impl IntoOperation,
        description: impl Into<String>,
    ) -> Result<ToolDraft, ToolCatalogBuildError>
    where
        Args: schemars::JsonSchema,
        Output: schemars::JsonSchema,
    {
        Self::raw(
            id,
            operation,
            description,
            schema_for::<Args>(),
            schema_for::<Output>(),
        )
    }

    pub fn with_argument_projection(
        mut self,
        argument_projection: ToolArgumentProjectionPolicy,
    ) -> Self {
        self.manifest.argument_projection = argument_projection;
        self
    }

    pub fn with_execution_policy(mut self, execution_policy: ExecutionPolicy) -> Self {
        self.manifest.execution_policy = execution_policy;
        self
    }

    /// Sets another execution bound: how long one run of the tool's body
    /// may take.
    pub fn with_execution(mut self, execution: std::time::Duration) -> Self {
        self.manifest = self.manifest.with_execution(execution);
        self
    }

    /// Declares what the tool's inline body may do beyond a Done result:
    /// return Deferred, or declare Lash intents. `park` is how long a call
    /// may stay parked waiting for its completion, which the host of a tool
    /// that may defer sets, and no other tool has.
    ///
    /// # Errors
    ///
    /// [`RegistrationRefused`]: an invalid declaration, a deferring tool
    /// without a park bound, a park bound on a tool that never defers, or an
    /// isolated declaration, which [`Self::isolated_in`] makes; or a turn
    /// control on a tool that [`Self::control`] did not define. The
    /// controls a [`Self::control`] definition states are kept.
    pub fn with_declaration(
        mut self,
        mut declaration: ToolDeclaration,
        park: Option<ParkBound>,
    ) -> Result<Self, RegistrationRefused> {
        let defined = &self.manifest.declaration.controls;
        if defined.is_empty() && !declaration.controls.is_empty() {
            return Err(RegistrationRefused::ControlDeclaresOutput {
                tool: self.manifest.tool(),
            });
        }
        if let Some(finish) = defined.finish_declaration() {
            declaration.controls = declaration
                .controls
                .with_finish(finish.value_schema.clone());
        }
        if defined.contains(crate::TurnControlKind::SwitchAgentFrame) {
            declaration.controls = declaration.controls.with_switch_agent_frame();
        }
        self.manifest = self.manifest.declared(declaration, park, None)?;
        Ok(self)
    }

    /// This finish tool typed by `value_schema`: its input, and the value of
    /// every Finish it emits, must match it. Its id, operation, bounds
    /// and policy stay, so the provider that runs it runs it unchanged: a
    /// run that offers it so through its restricted tool access changes the
    /// final value's shape for that run alone. `None` when the tool declares
    /// no Finish.
    #[must_use]
    pub fn with_finish_value_schema(mut self, value_schema: JsonSchema) -> Option<Self> {
        self.manifest.declaration.controls.finish_declaration()?;
        self.manifest.declaration.controls = self
            .manifest
            .declaration
            .controls
            .clone()
            .with_finish(value_schema.clone());
        self.contract = ToolContract {
            input_schema: SchemaContract::new(value_schema),
            ..self.contract.clone()
        };
        Some(self)
    }

    /// This tool as a call of it was admitted: declaring `declaration`, the
    /// declaration its admission recorded, so the call settles under it
    /// whatever the live definition declares now.
    #[must_use]
    pub fn with_admitted_declaration(mut self, declaration: ToolDeclaration) -> Self {
        self.manifest.declaration = declaration;
        self
    }

    /// Declares the tool isolated: every call runs as a process of the
    /// engine registered as `engine`, with no inline body.
    ///
    /// # Errors
    ///
    /// [`RegistrationRefused`]: the tool already declares what only an
    /// inline body does.
    pub fn isolated_in(mut self, engine: impl Into<String>) -> Result<Self, RegistrationRefused> {
        let declaration = ToolDeclaration {
            isolated: true,
            ..self.manifest.declaration.clone()
        };
        let park = self.manifest.park;
        self.manifest = self
            .manifest
            .declared(declaration, park, Some(engine.into()))?;
        Ok(self)
    }

    pub fn with_output_contract(mut self, output_contract: ToolOutputContract) -> Self {
        self.contract.output_contract = output_contract;
        self
    }

    pub fn with_input_schema_projection(
        mut self,
        dialect: SchemaDialect,
        schema: JsonSchema,
    ) -> Self {
        self.contract
            .input_schema
            .projection
            .set_override(SchemaProjectionOverride::new(dialect, schema));
        self
    }

    pub fn with_output_schema_projection(
        mut self,
        dialect: SchemaDialect,
        schema: JsonSchema,
    ) -> Self {
        self.contract
            .output_schema
            .projection
            .set_override(SchemaProjectionOverride::new(dialect, schema));
        self
    }

    pub fn with_output_from_input_schema(
        self,
        input_field: impl Into<String>,
        default_schema: Option<JsonSchema>,
    ) -> Self {
        self.with_output_contract(ToolOutputContract::from_input_schema(
            input_field,
            default_schema,
        ))
    }

    pub fn default_input_schema() -> serde_json::Value {
        ToolContract::default_input_schema()
    }

    /// Tool identity.
    pub fn id(&self) -> &ToolId {
        &self.manifest.id
    }

    /// The operation a document performs the tool as.
    pub fn operation(&self) -> &QualifiedOperation {
        &self.manifest.operation
    }

    /// Tool description.
    pub fn description(&self) -> &str {
        &self.manifest.description
    }

    pub fn model_tool(&self) -> ModelTool {
        self.contract.model_tool(&self.manifest)
    }

    /// The manifest projection: what a provider advertises.
    pub fn manifest(&self) -> ToolManifest {
        self.manifest.clone()
    }

    pub fn contract(&self) -> ToolContract {
        let mut contract = self.contract.clone();
        contract.identity = Some(ToolContractIdentity {
            id: self.manifest.id.clone(),
            operation: self.manifest.operation.clone(),
        });
        contract
    }

    /// Recompose a definition from its [`ToolManifest`] and [`ToolContract`]
    /// projections — the inverse of [`ToolDefinition::manifest`]/[`ToolDefinition::contract`].
    pub fn from_parts(manifest: ToolManifest, mut contract: ToolContract) -> Self {
        contract.identity = Some(ToolContractIdentity {
            id: manifest.id.clone(),
            operation: manifest.operation.clone(),
        });
        Self { manifest, contract }
    }
}

/// The JSON Schema of `T`, as schemars generates it.
pub fn schema_for<T>() -> serde_json::Value
where
    T: schemars::JsonSchema,
{
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| serde_json::json!({}))
}

#[cfg(feature = "schema-validation")]
mod schema_validation;
#[cfg(feature = "schema-validation")]
pub use schema_validation::validate_tool_input;

#[cfg(test)]
mod authored_examples_tests {
    use super::*;

    /// FIG-6490: removed authored examples are refused, even when empty,
    /// rather than silently discarded at a tool's wire boundary.
    #[test]
    fn legacy_authored_examples_are_refused_at_the_contract_boundary() {
        for examples in [serde_json::json!([]), serde_json::json!(["old call"])] {
            let contract = serde_json::json!({ "examples": examples });
            let refused = serde_json::from_value::<ToolContract>(contract.clone())
                .expect_err("authored examples are no longer a contract member");
            assert!(
                refused.to_string().contains("unknown field `examples`"),
                "{refused}"
            );
            let definition = serde_json::json!({
                "manifest": { "id": "tool:legacy", "operation": "tools.legacy", "execution": { "secs": 30, "nanos": 0 } },
                "contract": contract,
            });
            let refused = serde_json::from_value::<ToolDefinition>(definition)
                .expect_err("nested contracts enforce the same wire boundary");
            assert!(
                refused.to_string().contains("unknown field `examples`"),
                "{refused}"
            );
        }
        let mut definition = serde_json::json!({
            "manifest": { "id": "tool:legacy", "operation": "tools.legacy", "execution": { "secs": 30, "nanos": 0 } },
            "contract": {},
        });
        serde_json::from_value::<ToolDefinition>(definition.clone())
            .expect("a current definition decodes");
        definition["examples"] = serde_json::json!([]);
        let refused = serde_json::from_value::<ToolDefinition>(definition)
            .expect_err("top-level legacy examples are refused too");
        assert!(
            refused.to_string().contains("unknown field `examples`"),
            "{refused}"
        );
        let schema = serde_json::to_value(schemars::schema_for!(ToolContract)).expect("schema");
        assert!(schema["properties"].get("examples").is_none());
    }
}
