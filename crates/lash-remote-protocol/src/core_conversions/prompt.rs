use super::*;

impl From<lash_core::PromptLayer> for RemotePromptLayer {
    fn from(value: lash_core::PromptLayer) -> Self {
        let lash_core::PromptLayer { template, slots } = value;
        Self {
            template: template.map(Into::into),
            slots: slots
                .into_iter()
                .map(|(slot, layer)| (slot.into(), layer.into()))
                .collect(),
        }
    }
}

impl From<RemotePromptLayer> for lash_core::PromptLayer {
    fn from(value: RemotePromptLayer) -> Self {
        let RemotePromptLayer { template, slots } = value;
        Self {
            template: template.map(Into::into),
            slots: slots
                .into_iter()
                .map(|(slot, layer)| (slot.into(), layer.into()))
                .collect(),
        }
    }
}

impl From<lash_core::PromptTemplate> for RemotePromptTemplate {
    fn from(value: lash_core::PromptTemplate) -> Self {
        let lash_core::PromptTemplate { sections } = value;
        Self {
            sections: sections.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<RemotePromptTemplate> for lash_core::PromptTemplate {
    fn from(value: RemotePromptTemplate) -> Self {
        let RemotePromptTemplate { sections } = value;
        Self {
            sections: sections.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<lash_core::PromptTemplateSection> for RemotePromptTemplateSection {
    fn from(value: lash_core::PromptTemplateSection) -> Self {
        let lash_core::PromptTemplateSection { title, entries } = value;
        Self {
            title,
            entries: entries.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<RemotePromptTemplateSection> for lash_core::PromptTemplateSection {
    fn from(value: RemotePromptTemplateSection) -> Self {
        let RemotePromptTemplateSection { title, entries } = value;
        Self {
            title,
            entries: entries.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<lash_core::PromptTemplateEntry> for RemotePromptTemplateEntry {
    fn from(value: lash_core::PromptTemplateEntry) -> Self {
        match value {
            lash_core::PromptTemplateEntry::Text { content } => Self::Text { content },
            lash_core::PromptTemplateEntry::Builtin { builtin } => Self::Builtin {
                builtin: builtin.into(),
            },
            lash_core::PromptTemplateEntry::Slot { slot } => Self::Slot { slot: slot.into() },
        }
    }
}

impl From<RemotePromptTemplateEntry> for lash_core::PromptTemplateEntry {
    fn from(value: RemotePromptTemplateEntry) -> Self {
        match value {
            RemotePromptTemplateEntry::Text { content } => Self::Text { content },
            RemotePromptTemplateEntry::Builtin { builtin } => Self::Builtin {
                builtin: builtin.into(),
            },
            RemotePromptTemplateEntry::Slot { slot } => Self::Slot { slot: slot.into() },
        }
    }
}

impl From<lash_core::PromptBuiltin> for RemotePromptBuiltin {
    fn from(value: lash_core::PromptBuiltin) -> Self {
        match value {
            lash_core::PromptBuiltin::MainAgentIntro => Self::MainAgentIntro,
            lash_core::PromptBuiltin::ExecutionInstructions => Self::ExecutionInstructions,
            lash_core::PromptBuiltin::CoreGuidance => Self::CoreGuidance,
        }
    }
}

impl From<RemotePromptBuiltin> for lash_core::PromptBuiltin {
    fn from(value: RemotePromptBuiltin) -> Self {
        match value {
            RemotePromptBuiltin::MainAgentIntro => Self::MainAgentIntro,
            RemotePromptBuiltin::ExecutionInstructions => Self::ExecutionInstructions,
            RemotePromptBuiltin::CoreGuidance => Self::CoreGuidance,
        }
    }
}

impl From<lash_core::PromptSlot> for RemotePromptSlot {
    fn from(value: lash_core::PromptSlot) -> Self {
        match value {
            lash_core::PromptSlot::Intro => Self::Intro,
            lash_core::PromptSlot::Execution => Self::Execution,
            lash_core::PromptSlot::Guidance => Self::Guidance,
            lash_core::PromptSlot::ProjectInstructions => Self::ProjectInstructions,
            lash_core::PromptSlot::RuntimeContext => Self::RuntimeContext,
            lash_core::PromptSlot::Environment => Self::Environment,
        }
    }
}

impl From<RemotePromptSlot> for lash_core::PromptSlot {
    fn from(value: RemotePromptSlot) -> Self {
        match value {
            RemotePromptSlot::Intro => Self::Intro,
            RemotePromptSlot::Execution => Self::Execution,
            RemotePromptSlot::Guidance => Self::Guidance,
            RemotePromptSlot::ProjectInstructions => Self::ProjectInstructions,
            RemotePromptSlot::RuntimeContext => Self::RuntimeContext,
            RemotePromptSlot::Environment => Self::Environment,
        }
    }
}

impl From<lash_core::PromptSlotLayer> for RemotePromptSlotLayer {
    fn from(value: lash_core::PromptSlotLayer) -> Self {
        let lash_core::PromptSlotLayer {
            reset,
            contributions,
        } = value;
        Self {
            reset,
            contributions: contributions.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<RemotePromptSlotLayer> for lash_core::PromptSlotLayer {
    fn from(value: RemotePromptSlotLayer) -> Self {
        let RemotePromptSlotLayer {
            reset,
            contributions,
        } = value;
        Self {
            reset,
            contributions: contributions.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<lash_core::PromptContribution> for RemotePromptContribution {
    fn from(value: lash_core::PromptContribution) -> Self {
        let lash_core::PromptContribution {
            slot,
            title,
            priority,
            gate,
            content,
        } = value;
        Self {
            slot: slot.into(),
            title: title.map(|title| title.to_string()),
            priority,
            gate: gate.into(),
            content: content.to_string(),
        }
    }
}

impl From<RemotePromptContribution> for lash_core::PromptContribution {
    fn from(value: RemotePromptContribution) -> Self {
        let RemotePromptContribution {
            slot,
            title,
            priority,
            gate,
            content,
        } = value;
        Self {
            slot: slot.into(),
            title: title.map(Arc::from),
            priority,
            gate: gate.into(),
            content: Arc::from(content),
        }
    }
}

impl From<lash_core::PromptContributionGate> for RemotePromptContributionGate {
    fn from(value: lash_core::PromptContributionGate) -> Self {
        let lash_core::PromptContributionGate { tools } = value;
        Self { tools }
    }
}

impl From<RemotePromptContributionGate> for lash_core::PromptContributionGate {
    fn from(value: RemotePromptContributionGate) -> Self {
        let RemotePromptContributionGate { tools } = value;
        Self { tools }
    }
}
