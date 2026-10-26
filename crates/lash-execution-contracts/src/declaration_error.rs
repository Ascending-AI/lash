use lash_kernel_value::providers::FormatVersion;

/// Why a factory's [`PluginDeclaration`] is refused.
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
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum PluginDeclarationError {
    /// Two factories cannot own the same plugin namespace.
    #[error("plugin id `{id}` is registered twice")]
    DuplicatePluginId { id: String },
    /// The declaration names another plugin than the factory that gave it.
    #[error("plugin factory `{factory}` declares itself as `{declared}`")]
    IdMismatch { factory: String, declared: String },
    /// The plugin cannot read the stream state its callbacks write.
    #[error("plugin `{plugin}` does not read its native stream-state format {format_version}")]
    NativeStreamStateNotReadable {
        plugin: String,
        #[schemars(with = "std::num::NonZeroU32")]
        format_version: FormatVersion,
    },
    /// The plugin cannot write the format it reads natively.
    #[error(
        "plugin `{plugin}` reads format {format_version} natively and does not declare it writable"
    )]
    NativeFormatNotWritable {
        plugin: String,
        #[schemars(with = "std::num::NonZeroU32")]
        format_version: FormatVersion,
    },
}
