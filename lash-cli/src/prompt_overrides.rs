use std::path::PathBuf;

use anyhow::anyhow;
use lash_core::agent::{PromptOverrideMode, PromptSectionName, PromptSectionOverride};

use crate::Args;

fn parse_section(raw: &str) -> anyhow::Result<PromptSectionName> {
    raw.parse::<PromptSectionName>().map_err(anyhow::Error::msg)
}

fn parse_section_kv(raw: &str) -> anyhow::Result<(PromptSectionName, String)> {
    let Some((section_raw, value)) = raw.split_once('=') else {
        return Err(anyhow!(
            "Expected SECTION=VALUE, got `{raw}`. Valid sections: {}",
            PromptSectionName::names_csv()
        ));
    };
    let section = parse_section(section_raw.trim())?;
    Ok((section, value.to_string()))
}

fn parse_section_file_kv(raw: &str) -> anyhow::Result<(PromptSectionName, PathBuf)> {
    let Some((section_raw, path_raw)) = raw.split_once('=') else {
        return Err(anyhow!(
            "Expected SECTION=PATH, got `{raw}`. Valid sections: {}",
            PromptSectionName::names_csv()
        ));
    };
    let section = parse_section(section_raw.trim())?;
    Ok((section, PathBuf::from(path_raw)))
}

fn read_override_file(
    path: &std::path::Path,
    section: PromptSectionName,
) -> anyhow::Result<String> {
    std::fs::read_to_string(path).map_err(|e| {
        anyhow!(
            "Failed reading override file {} for section {}: {}",
            path.display(),
            section.as_str(),
            e
        )
    })
}

pub fn resolve_prompt_overrides(args: &Args) -> anyhow::Result<Vec<PromptSectionOverride>> {
    let mut overrides = Vec::new();

    for raw in &args.prompt_replace {
        let (section, content) = parse_section_kv(raw)?;
        overrides.push(PromptSectionOverride {
            section,
            mode: PromptOverrideMode::Replace,
            content,
        });
    }
    for raw in &args.prompt_replace_file {
        let (section, path) = parse_section_file_kv(raw)?;
        overrides.push(PromptSectionOverride {
            section,
            mode: PromptOverrideMode::Replace,
            content: read_override_file(&path, section)?,
        });
    }

    for raw in &args.prompt_prepend {
        let (section, content) = parse_section_kv(raw)?;
        overrides.push(PromptSectionOverride {
            section,
            mode: PromptOverrideMode::Prepend,
            content,
        });
    }
    for raw in &args.prompt_prepend_file {
        let (section, path) = parse_section_file_kv(raw)?;
        overrides.push(PromptSectionOverride {
            section,
            mode: PromptOverrideMode::Prepend,
            content: read_override_file(&path, section)?,
        });
    }

    for raw in &args.prompt_append {
        let (section, content) = parse_section_kv(raw)?;
        overrides.push(PromptSectionOverride {
            section,
            mode: PromptOverrideMode::Append,
            content,
        });
    }
    for raw in &args.prompt_append_file {
        let (section, path) = parse_section_file_kv(raw)?;
        overrides.push(PromptSectionOverride {
            section,
            mode: PromptOverrideMode::Append,
            content: read_override_file(&path, section)?,
        });
    }

    for raw in &args.prompt_disable {
        let section = parse_section(raw)?;
        overrides.push(PromptSectionOverride {
            section,
            mode: PromptOverrideMode::Disable,
            content: String::new(),
        });
    }

    Ok(overrides)
}
