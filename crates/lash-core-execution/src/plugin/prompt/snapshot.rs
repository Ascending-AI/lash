//! Pure prompt descriptors shared by host readers and config validation.
use super::*;
use crate::plugin::{ContributionDetails, ContributionKey, InstalledInventory};

/// The installed prompt declarations. Dynamic families are listed by prefix;
/// reading this value never expands them or binds a renderer.
#[derive(Clone, Debug, Default)]
pub struct PromptCatalogSnapshot {
    sections: Vec<PromptSectionInfo>,
    families: Vec<PromptSectionFamilyInfo>,
    wraps: Vec<ResolvedPromptWrap>,
}

impl PromptCatalogSnapshot {
    pub fn from_inventory(inventory: &InstalledInventory) -> Self {
        let mut catalog = Self::default();
        for entry in &inventory.entries {
            match (&entry.key, &entry.details) {
                (ContributionKey::PromptSection(id), ContributionDetails::PromptSection(spec)) => {
                    catalog.sections.push(PromptSectionInfo {
                        section: id.clone(),
                        owner: id.owner.clone(),
                        default_placement: spec.default_placement,
                        purposes: spec.purposes.clone(),
                    })
                }
                (ContributionKey::PromptFamily(id), ContributionDetails::PromptFamily(spec)) => {
                    catalog.families.push(PromptSectionFamilyInfo {
                        family: id.clone(),
                        owner: id.owner.clone(),
                        default_placement: spec.default_placement,
                        purposes: spec.purposes.clone(),
                    })
                }
                (ContributionKey::PromptWrapper(id), ContributionDetails::PromptWrapper(spec)) => {
                    catalog.wraps.push(ResolvedPromptWrap {
                        wrap: id.clone(),
                        owner: id.owner.clone(),
                        target: spec.target.clone(),
                        ordinal: u32::try_from(catalog.wraps.len()).unwrap_or(u32::MAX),
                    })
                }
                _ => {}
            }
        }
        catalog
    }
    pub fn sections(&self) -> Vec<PromptSectionInfo> {
        self.sections.clone()
    }
    pub fn families(&self) -> Vec<PromptSectionFamilyInfo> {
        self.families.clone()
    }
    pub fn wraps(&self) -> Vec<ResolvedPromptWrap> {
        self.wraps.clone()
    }
    pub fn validate_plan(&self, plan: &PromptPlan) -> Result<(), PromptPlanError> {
        plan.validate()?;
        for section in plan
            .order
            .iter()
            .chain(plan.placements.iter().map(|p| &p.section))
        {
            let known = self.sections.iter().any(|entry| entry.section == *section)
                || self.families.iter().any(|entry| {
                    entry.family.owner == section.owner
                        && section
                            .key
                            .as_str()
                            .strip_prefix(entry.family.key.as_str())
                            .is_some_and(|tail| tail.starts_with('.') && tail.len() > 1)
                });
            if !known {
                return Err(PromptPlanError::UnknownSection {
                    section: section.clone(),
                });
            }
        }
        Ok(())
    }
}

/// The one durable prompt-plan read, shared by diagnostics and session admin.
pub async fn read_prompt_plan(
    store: &crate::store::SessionStore,
) -> Result<Option<PromptPlan>, crate::StoreError> {
    Ok(store
        .load_session_head_meta()
        .await?
        .map(|head| head.config.prompt_plan))
}
