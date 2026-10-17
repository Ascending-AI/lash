//! FIG-6162: a plain tool round's outcomes commit with the next model start
//! whatever plugins the session installs. A plugin's prompt section renders
//! from the turn's live state, so it costs the round no commit of its own:
//! a session with a third-party plugin's static section, or with the
//! delegation example, commits exactly what a bare session commits. Cut at
//! that fused commit, the round keeps exactly one outcome, and every model
//! request, a resumed turn's included, carries the section once.

use super::*;

const SECTIONED: &str = "sectioned";
/// The text of the third-party plugin's one section.
const SECTION: &str = "[sectioned: a static section]";

/// What the deployment installs beside the scenario's own plugins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Installed {
    #[default]
    Nothing,
    /// A third-party plugin whose one prompt section is static text.
    Section,
    /// The delegation example (`examples/delegation`), whose child-only
    /// section is registered on every session it is installed in.
    Delegation,
}

impl Installed {
    pub(super) fn install(self, builder: lash::LashCoreBuilder) -> lash::LashCoreBuilder {
        match self {
            Self::Nothing => builder,
            Self::Section => builder.plugin(Arc::new(Sectioned)),
            Self::Delegation => builder.plugin(Arc::new(delegation::DelegationPluginFactory::new(
                served::spec(16),
                lash_execution_contracts::process::lifetime::starter,
            ))),
        }
    }

    /// Every model request carries an installed section exactly once.
    pub(super) fn laws(self, requests: &[String]) -> Vec<String> {
        if self != Self::Section {
            return Vec::new();
        }
        requests
            .iter()
            .enumerate()
            .filter_map(|(index, request)| {
                let shown = request.matches(SECTION).count();
                (shown != 1).then(|| {
                    format!("model request {index} carries the plugin's section {shown} times")
                })
            })
            .collect()
    }
}

/// A third-party plugin with one static section and nothing else.
struct Sectioned;

impl lash::plugins::PluginMetadata for Sectioned {
    fn plugin_declaration(&self) -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(SECTIONED)
    }
}

impl lash::plugins::PluginFactory for Sectioned {
    fn id(&self) -> &'static str {
        SECTIONED
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<
        lash_core::plugin::PluginRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let mut reg = lash_core::plugin::PluginRegistrar::<Sectioned>::new(SECTIONED);
        reg.prompt().section(
            lash::plugins::PromptSectionSpec::new(
                lash::prompt::PromptSectionKey::new(SECTIONED)
                    .map_err(|error| lash::plugins::PluginError::Registration(error.to_string()))?,
                lash::prompt::PromptPlacement::CurrentContext,
            ),
            |_this, _state| {
                Ok(Arc::new(|_input: &lash::plugins::PromptInput<'_>| {
                    Ok(lash::plugins::SectionText::text(SECTION))
                }))
            },
        )?;
        Ok(lash_core::plugin::PluginRecipe::new(
            SECTIONED,
            reg,
            |_ctx, _state| Ok(Arc::new(Sectioned)),
        ))
    }
}

impl lash::plugins::SessionPlugin for Sectioned {
    fn id(&self) -> &'static str {
        SECTIONED
    }
}

/// The owner's committed labels for `turn` with `installed`, uncut, through
/// its host answer.
async fn owner_labels(turn: Turn, installed: Installed) -> Vec<CommitLabel> {
    let report = Matrix::new()
        .faults(&[])
        .run_test(|| {
            let mut crash = Crash::new(turn, Dialect::SqliteMemory, None);
            crash.installed = installed;
            crash
        })
        .await;
    report.assert_held();
    let all: Vec<_> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    let answer = all
        .iter()
        .position(|label| *label == CommitLabel::TURN_COMMIT)
        .expect("the turn committed its host answer");
    all[..=answer].to_vec()
}

#[tokio::test]
async fn a_plain_tool_round_commits_with_the_next_model_start_whatever_plugins_are_installed() {
    let bare = owner_labels(Turn::Activity, Installed::Nothing).await;
    assert_eq!(
        bare.iter()
            .filter(|label| **label == CommitLabel::ROUND_OUTCOME_PRESENT_MODEL_START)
            .count(),
        1,
        "the round's outcome rides the next model start: {bare:?}"
    );
    assert!(!bare.contains(&CommitLabel::ROUND_OUTCOME), "{bare:?}");
    for installed in [Installed::Section, Installed::Delegation] {
        assert_eq!(
            owner_labels(Turn::Activity, installed).await,
            bare,
            "{installed:?} costs the round no commit"
        );
    }
}

/// A sectioned session's plain round cut at the commit that carries its
/// outcome and its next model start, under every fault, keeps exactly one
/// outcome for its call, and the resumed turn shows the model the section
/// as the first owner did.
pub(super) async fn a_sectioned_round_cut_at_its_fused_commit_keeps_one_outcome_and_its_section(
    tier: Tier,
) {
    prove_installed(
        Turn::Activity,
        tier,
        &[CommitLabel::ROUND_OUTCOME_PRESENT_MODEL_START],
        &[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ],
        Installed::Section,
    )
    .await;
}
