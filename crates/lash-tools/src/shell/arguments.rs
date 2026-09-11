//! Typed input contracts shared by the shell parsers and JSON Schemas.

use lash_core::{ProcessId, ToolOutcome};
use schemars::JsonSchema;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{DeserializeOwned, Error as _},
};
use serde_json::{Value, json};

use lash_tool_support::{invalid_tool_args, non_empty_string, typed_args};

use super::runtime::DEFAULT_EXEC_COMMAND_TIMEOUT_MS;

/// Fields shared by every command-launching shell tool.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct ShellCommandArgs<L> {
    pub(super) cmd: String,
    /// Optional working directory to run the command in; defaults to the turn cwd.
    #[serde(
        default,
        deserialize_with = "deserialize_optional_string",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub(super) workdir: Option<String>,
    /// Shell binary to launch. Defaults to the user's default shell.
    #[serde(
        default,
        deserialize_with = "deserialize_optional_string",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub(super) shell: Option<String>,
    /// Whether to run the shell with -l semantics. Defaults to false to avoid startup prompts and shell init noise.
    #[serde(default, deserialize_with = "deserialize_default_bool")]
    pub(super) login: bool,
    /// Maximum number of tokens to return. Excess output will be truncated.
    #[serde(
        default,
        deserialize_with = "deserialize_optional_usize",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "u64", range(min = 1))]
    pub(super) max_output_tokens: Option<usize>,
    #[serde(flatten)]
    pub(super) lane: L,
}

/// Extra arguments accepted by `exec_command`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct ExecCommandLane {
    #[serde(
        default = "default_exec_command_timeout_ms",
        deserialize_with = "deserialize_timeout_ms"
    )]
    #[schemars(default = "default_exec_command_timeout_ms", range(min = 1))]
    pub(super) timeout_ms: u64,
}

/// Arguments shared by both the public and internal start-command lanes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct StartCommandLane<I> {
    /// Launch the command fully detached (its own session via setsid) so it outlives this session and host. lash records only an immediately-terminal audit fact and never tracks, signals, or stops it. Defaults to false (a tracked PTY process).
    #[serde(default, deserialize_with = "deserialize_default_bool")]
    pub(super) detach: bool,
    #[serde(flatten)]
    pub(super) internal: I,
}

/// The public lane deliberately contributes no internal fields.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub(super) struct PublicStartCommandLane {}

/// Fields carried only by the sealed `run_start_command` process body.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub(super) struct InternalStartCommandLane {
    #[serde(
        default,
        deserialize_with = "deserialize_optional_process_id",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub(super) detached_process_id: Option<ProcessId>,
}

pub(super) type ExecCommandArgs = ShellCommandArgs<ExecCommandLane>;
pub(super) type PublicStartCommandArgs = ShellCommandArgs<StartCommandLane<PublicStartCommandLane>>;
pub(super) type InternalStartCommandArgs =
    ShellCommandArgs<StartCommandLane<InternalStartCommandLane>>;

pub(super) trait ShellCommandLane: JsonSchema {
    const COMMAND_DESCRIPTION: &'static str;

    fn customize_schema(_schema: &mut Value) {}

    fn validate_direct_input(_value: &Value) -> Result<(), ToolOutcome> {
        Ok(())
    }
}

impl ShellCommandLane for ExecCommandLane {
    const COMMAND_DESCRIPTION: &'static str = "Shell command to execute.";

    fn customize_schema(schema: &mut Value) {
        schema["properties"]["timeout_ms"]
            .as_object_mut()
            .expect("typed timeout schema is an object")
            .remove("format");
        schema["properties"]["timeout_ms"]["minimum"] = json!(1);
        schema["properties"]["timeout_ms"]["description"] = json!(format!(
            "Hard timeout in milliseconds. If reached before the command exits, the process is killed and returned as a tool failure with `status: \"timed_out\"` and `timed_out: true`. Defaults to {DEFAULT_EXEC_COMMAND_TIMEOUT_MS} ms."
        ));
    }
}

pub(super) trait StartCommandInternalLane: JsonSchema {
    const PUBLIC: bool;
}

impl StartCommandInternalLane for PublicStartCommandLane {
    const PUBLIC: bool = true;
}

impl StartCommandInternalLane for InternalStartCommandLane {
    const PUBLIC: bool = false;
}

impl<I> ShellCommandLane for StartCommandLane<I>
where
    I: StartCommandInternalLane,
{
    const COMMAND_DESCRIPTION: &'static str = "Shell command to start.";

    fn customize_schema(schema: &mut Value) {
        if !I::PUBLIC {
            schema["properties"]["detach"]
                .as_object_mut()
                .expect("typed detach schema is an object")
                .remove("description");
        }
    }

    fn validate_direct_input(value: &Value) -> Result<(), ToolOutcome> {
        if I::PUBLIC && value.get("detached_process_id").is_some() {
            return Err(invalid_tool_args(
                "Invalid tool arguments: unknown field `detached_process_id`",
            ));
        }
        Ok(())
    }
}

impl<L> ShellCommandArgs<L>
where
    L: ShellCommandLane,
{
    /// Derive the closed model-facing schema from the typed canonical fields.
    pub(super) fn schema() -> Value {
        let mut schema = serde_json::to_value(schemars::schema_for!(ShellCommandArgs<L>))
            .expect("typed shell argument schemas serialize to JSON");
        if let Some(object) = schema.as_object_mut() {
            object.remove("$schema");
            object.remove("title");
            object.remove("description");
            object.insert("additionalProperties".to_string(), json!(false));
        }
        schema["properties"]["max_output_tokens"]
            .as_object_mut()
            .expect("typed output-token schema is an object")
            .remove("format");
        schema["properties"]["max_output_tokens"]["minimum"] = json!(1);
        schema["properties"]["cmd"]["description"] = json!(L::COMMAND_DESCRIPTION);
        L::customize_schema(&mut schema);
        schema
    }
}

impl<L> ShellCommandArgs<L>
where
    L: ShellCommandLane + DeserializeOwned,
{
    /// Parse canonical fields plus the legacy spellings accepted by direct callers.
    ///
    /// Model-facing calls still validate against [`Self::schema`] first. Direct
    /// provider calls historically normalize null defaults and ignore unknown
    /// fields, so deserialization retains that compatibility without widening
    /// the generated schema. Lane validation separately protects fields whose
    /// rejection is an authority boundary.
    pub(super) fn parse(value: &Value) -> Result<Self, ToolOutcome> {
        L::validate_direct_input(value)?;
        let parsed: Self = typed_args(value)?;
        non_empty_string(&parsed.cmd, "cmd")?;
        if parsed.max_output_tokens == Some(0) {
            return Err(invalid_tool_args(
                "Invalid max_output_tokens: must be >= 1, or use null/\"none\" for no cap",
            ));
        }
        Ok(parsed)
    }
}

fn default_exec_command_timeout_ms() -> u64 {
    DEFAULT_EXEC_COMMAND_TIMEOUT_MS
}

fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(|value| value.as_str().map(ToOwned::to_owned))
}

fn deserialize_default_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<bool>::deserialize(deserializer).map(Option::unwrap_or_default)
}

fn deserialize_optional_usize<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::Null => Ok(None),
        Value::String(value) if value.eq_ignore_ascii_case("none") => Ok(None),
        Value::Number(value) => value
            .as_u64()
            .map(|value| Some(value as usize))
            .ok_or_else(|| D::Error::custom("expected a positive integer, null, or \"none\"")),
        _ => Err(D::Error::custom(
            "expected a positive integer, null, or \"none\"",
        )),
    }
}

fn deserialize_timeout_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    u64::deserialize(deserializer).map(|value| value as usize as u64)
}

fn deserialize_optional_process_id<'de, D>(deserializer: D) -> Result<Option<ProcessId>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(|value| value.as_str().map(ProcessId::from))
}
