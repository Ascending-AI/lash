//! Where each contribution may be installed (FIG-6273): one table, read
//! twice.
//!
//! At compile time the table decides which [`PluginRegistrar`] methods a
//! registry's registrar offers: the registry is a type parameter, and a
//! method exists for it only when the table admits the method's
//! contribution there. A process recipe that declares a turn hook does not
//! compile.
//!
//! At run time the same table judges descriptors that arrive as data rather
//! than through the typed builder: a session plugin's spec offered as a
//! process component, and the restriction a row places on a contribution's
//! own details (a process prompt section answers direct purposes only).
//!
//! A contribution a registration emits with another, such as the reader
//! [`CallbackSlot::ToolReads`] a provider declares beside itself, `follows`
//! its parent: it is installed wherever the parent is, by construction.
//!
//! [`PluginRegistrar`]: super::PluginRegistrar

use super::recipe::{ContributionDetails, ContributionKind};
use crate::prompt_sections::PromptPurpose;
use lash_core_store::obligations::RegistryKind;
use lash_core_store::tool_run::CallbackSlot;

/// A registry a [`PluginRegistrar`](super::PluginRegistrar) declares for,
/// as a type: [`Session`] or [`Process`]. A session recipe takes only a
/// [`Session`] registrar and a process recipe only a [`Process`] one, and
/// the table's [`Admits`] impls are this crate's alone.
pub trait Registry: Send + Sync + 'static {
    /// The registry as a value, as descriptors and provider handles carry it.
    const KIND: RegistryKind;
}

/// A session's plugins.
#[derive(Clone, Copy, Debug)]
pub enum Session {}

/// A process's environment components.
#[derive(Clone, Copy, Debug)]
pub enum Process {}

impl Registry for Session {
    const KIND: RegistryKind = RegistryKind::Session;
}

impl Registry for Process {
    const KIND: RegistryKind = RegistryKind::Process;
}

/// A contribution kind, as a type: one of [`contribution`]'s markers.
pub trait Contribution: 'static {
    const KIND: ContributionKind;
}

/// A callback contribution, by its slot.
pub trait CallbackContribution: Contribution {
    const SLOT: CallbackSlot;
}

/// The registry installs contributions of kind `C`: implemented exactly for
/// the registries the contribution table names.
#[diagnostic::on_unimplemented(
    message = "the `{Self}` registry does not install `{C}` contributions",
    label = "not offered to a `{Self}` registrar",
    note = "the contribution table in the plugin `registries` module lists where each contribution may be installed"
)]
pub trait Admits<C: Contribution>: Registry {}

/// A process prompt section answers direct purposes only: a process has no
/// turn to prepare.
fn direct_purposes_only(details: &ContributionDetails) -> bool {
    matches!(
        details,
        ContributionDetails::PromptSection(spec)
            if spec.purposes.iter().all(|purpose| matches!(purpose, PromptPurpose::Direct { .. }))
    )
}

macro_rules! contribution_table {
    (
        callbacks { $($slot:ident => $slot_rule:tt;)+ }
        others { $($kind:ident => $kind_rule:tt;)+ }
    ) => {
        /// Every contribution kind as a type, for the bounds of the
        /// registrar methods that declare it.
        pub mod contribution {
            use super::{CallbackContribution, CallbackSlot, Contribution, ContributionKind};
            $(
                #[doc = concat!("The `", stringify!($slot), "` callback slot.")]
                #[derive(Clone, Copy, Debug)]
                pub enum $slot {}
                impl Contribution for $slot {
                    const KIND: ContributionKind = ContributionKind::Callback(CallbackSlot::$slot);
                }
                impl CallbackContribution for $slot {
                    const SLOT: CallbackSlot = CallbackSlot::$slot;
                }
            )+
            $(
                #[doc = concat!("The `", stringify!($kind), "` contribution.")]
                #[derive(Clone, Copy, Debug)]
                pub enum $kind {}
                impl Contribution for $kind {
                    const KIND: ContributionKind = ContributionKind::$kind;
                }
            )+
        }
        $(contribution_table!(@impls $slot $slot_rule);)+
        $(contribution_table!(@impls $kind $kind_rule);)+

        /// Whether `registry` installs a contribution of `kind` with
        /// `details`: the table's runtime reading, for descriptors that
        /// arrive as data.
        #[must_use]
        pub fn admits(
            kind: &ContributionKind,
            details: &ContributionDetails,
            registry: RegistryKind,
        ) -> bool {
            match kind {
                $(ContributionKind::Callback(CallbackSlot::$slot) => {
                    contribution_table!(@admits registry details $slot_rule)
                })+
                $(ContributionKind::$kind => contribution_table!(@admits registry details $kind_rule),)+
            }
        }
    };
    (@impls $marker:ident [$($registry:ident $(where $condition:path)?),+]) => {
        $(impl Admits<contribution::$marker> for $registry {})+
    };
    (@impls $marker:ident (follows $parent:ident)) => {
        impl<R: Admits<contribution::$parent>> Admits<contribution::$marker> for R {}
    };
    (@admits $installing:ident $details:ident [$($registry:ident $(where $condition:path)?),+]) => {
        false $(|| ($installing == RegistryKind::$registry $(&& $condition($details))?))+
    };
    (@admits $installing:ident $details:ident (follows $parent:ident)) => {
        admits(&ContributionKind::Callback(CallbackSlot::$parent), $details, $installing)
    };
}

contribution_table! {
    callbacks {
        ToolProvider => [Session, Process];
        ToolReads => (follows ToolProvider);
        ToolCatalog => [Session, Process];
        BeforeTurn => [Session];
        ToolArgsTransform => [Session, Process];
        ToolArgsCheck => [Session, Process];
        ToolResultTransform => [Session, Process];
        ToolResultCheck => [Session, Process];
        AfterTurn => [Session];
        Checkpoint => [Session];
        AssistantStream => [Session];
        AssistantResponse => [Session];
        AssistantStreamFinished => [Session];
        PresentationStep => [Session, Process];
        PresentationPresenter => [Session, Process];
        RuntimeEvent => [Session];
        Operation => [Session];
        AttachmentOmission => [Session];
        ContextCompactor => [Session];
        ContextPressure => [Session];
        ProtocolSession => [Session];
        ProtocolDriver => [Session];
        CodeExecutor => [Session];
        AssistantProseProjector => [Session];
    }
    others {
        PromptSection => [Session, Process where direct_purposes_only];
        PromptFamily => [Session];
        PromptWrapper => [Session];
        StateReducer => [Session, Process];
        ConfigNamespace => [Session, Process];
        ConfigCommand => [Session];
        Extension => [Session];
        ProcessEngine => [Session];
        TranscriptDecoder => [Session];
    }
}
