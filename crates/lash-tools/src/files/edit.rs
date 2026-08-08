use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use unicode_normalization::UnicodeNormalization;

use lash_core::{ToolCall, ToolDefinition, ToolResult};

use lash_tool_support::{
    StaticToolExecute, StaticToolProvider, ToolDefinitionLashlangExt, compact_diff,
    display_relative, execute_typed_tool_result, invalid_request_failure, invalid_tool_args,
    io_failure, non_empty_string, resolve_under, run_blocking,
};

const EDIT_DESCRIPTION: &str = "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.";

#[derive(Default)]
pub struct Edit;

pub fn edit_provider() -> StaticToolProvider<Edit> {
    StaticToolProvider::new(vec![edit_tool_definition()], Edit)
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EditReplacement {
    /// Exact text for one targeted replacement.
    old_text: String,
    /// Replacement text for this targeted edit.
    new_text: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    /// Path to the file to edit (relative or absolute).
    path: String,
    /// One or more targeted replacements.
    edits: Vec<EditReplacement>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct EditDetails {
    /// Display-oriented unified diff, capped for model readability.
    diff: String,
    /// Full unified patch preview for the changed file.
    patch: String,
    /// Line number of the first change in the new file.
    #[serde(skip_serializing_if = "Option::is_none")]
    first_changed_line: Option<usize>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
struct EditOutput {
    summary: String,
    path: String,
    replacements: usize,
    details: EditDetails,
}

#[async_trait::async_trait]
impl StaticToolExecute for Edit {
    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        execute_typed_tool_result::<EditArgs, _, _>(call.args, |args| async move {
            if let Err(err) = validate_edit_args(&args) {
                return err;
            }
            run_blocking(move || edit_file(args)).await
        })
        .await
    }
}

fn edit_tool_definition() -> ToolDefinition {
    ToolDefinition::typed::<EditArgs, EditOutput>("tool:edit", "edit", EDIT_DESCRIPTION)
        .with_examples(vec![
            r#"await files.edit({ path: "src/main.rs", edits: [{ oldText: "old();", newText: "new();" }] })?"#.into(),
            r#"await files.edit({ path: "README.md", edits: [{ oldText: "alpha", newText: "ALPHA" }, { oldText: "omega", newText: "OMEGA" }] })?"#.into(),
        ])
        .with_lashlang_binding(lash_tool_support::lashlang_binding(
            ["files"],
            "edit",
            &["replace", "edit_file"],
        ))
}

fn validate_edit_args(args: &EditArgs) -> Result<(), ToolResult> {
    non_empty_string(&args.path, "path")?;
    if args.edits.is_empty() {
        return Err(invalid_tool_args(
            "Edit tool input is invalid. edits must contain at least one replacement.",
        ));
    }
    Ok(())
}

fn edit_file(args: EditArgs) -> ToolResult {
    if let Err(err) = validate_edit_args(&args) {
        return err;
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => {
            return io_failure(
                "current_dir_failed",
                format!("Failed to determine cwd: {err}"),
            );
        }
    };
    let absolute_path = resolve_under(&cwd, Path::new(&args.path));
    let display_path = display_relative(&cwd, &absolute_path);

    if let Err(err) = ensure_editable_file(&absolute_path, &args.path) {
        return err;
    }

    let raw_content = match std::fs::read_to_string(&absolute_path) {
        Ok(content) => content,
        Err(err) => {
            return io_failure(
                "read_file_failed",
                format!("Could not edit file: {}. {err}.", args.path),
            );
        }
    };

    let (bom, content) = strip_bom(&raw_content);
    let original_ending = detect_line_ending(content);
    let normalized_content = normalize_to_lf(content);
    let applied =
        match apply_edits_to_normalized_content(&normalized_content, &args.edits, &args.path) {
            Ok(applied) => applied,
            Err(err) => return invalid_request_failure("edit_not_applicable", err),
        };

    let final_content = format!(
        "{bom}{}",
        restore_line_endings(&applied.new_content, original_ending)
    );
    if let Err(err) = std::fs::write(&absolute_path, final_content) {
        return io_failure(
            "write_file_failed",
            format!("Could not edit file: {}. {err}.", args.path),
        );
    }

    let diff = compact_diff(
        &applied.base_content,
        &applied.new_content,
        &display_path,
        240,
    );
    let patch = compact_diff(
        &applied.base_content,
        &applied.new_content,
        &display_path,
        usize::MAX,
    );
    let replacements = args.edits.len();
    lash_tool_support::typed_tool_ok(EditOutput {
        summary: format!(
            "Successfully replaced {replacements} block(s) in {}.",
            args.path
        ),
        path: args.path,
        replacements,
        details: EditDetails {
            diff,
            patch,
            first_changed_line: first_changed_line(&applied.base_content, &applied.new_content),
        },
    })
}

fn ensure_editable_file(path: &Path, input_path: &str) -> Result<(), ToolResult> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(invalid_request_failure(
            "path_not_file",
            format!("Could not edit file: {input_path}. Path is not a file."),
        )),
        Err(err) => Err(io_failure(
            "file_metadata_failed",
            format!("Could not edit file: {input_path}. {err}."),
        )),
    }
}

#[derive(Clone, Debug)]
struct AppliedEdits {
    base_content: String,
    new_content: String,
}

#[derive(Clone, Debug)]
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

#[derive(Clone, Debug)]
struct FuzzyMatch {
    found: bool,
    index: usize,
    match_length: usize,
    used_fuzzy_match: bool,
}

#[derive(Clone, Debug)]
struct LineSpan {
    start: usize,
    end: usize,
}

fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[EditReplacement],
    path: &str,
) -> Result<AppliedEdits, String> {
    let normalized_edits = edits
        .iter()
        .map(|edit| EditReplacement {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect::<Vec<_>>();

    for (index, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(empty_old_text_error(path, index, normalized_edits.len()));
        }
    }

    let used_fuzzy_match = normalized_edits
        .iter()
        .map(|edit| fuzzy_find_text(normalized_content, &edit.old_text))
        .any(|matched| matched.used_fuzzy_match);
    let replacement_base_content = if used_fuzzy_match {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched_edits = Vec::new();
    for (index, edit) in normalized_edits.iter().enumerate() {
        let matched = fuzzy_find_text(&replacement_base_content, &edit.old_text);
        if !matched.found {
            return Err(not_found_error(path, index, normalized_edits.len()));
        }

        let occurrences = count_occurrences(&replacement_base_content, &edit.old_text);
        if occurrences > 1 {
            return Err(duplicate_error(
                path,
                index,
                normalized_edits.len(),
                occurrences,
            ));
        }

        matched_edits.push(MatchedEdit {
            edit_index: index,
            match_index: matched.index,
            match_length: matched.match_length,
            new_text: edit.new_text.clone(),
        });
    }

    matched_edits.sort_by_key(|edit| edit.match_index);
    for pair in matched_edits.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                previous.edit_index, current.edit_index
            ));
        }
    }

    let base_content = normalized_content.to_string();
    let new_content = if used_fuzzy_match {
        apply_replacements_preserving_unchanged_lines(
            normalized_content,
            &replacement_base_content,
            &matched_edits,
        )?
    } else {
        apply_replacements(&replacement_base_content, &matched_edits, 0)
    };

    if base_content == new_content {
        return Err(no_change_error(path, normalized_edits.len()));
    }

    Ok(AppliedEdits {
        base_content,
        new_content,
    })
}

fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatch {
    if let Some(index) = content.find(old_text) {
        return FuzzyMatch {
            found: true,
            index,
            match_length: old_text.len(),
            used_fuzzy_match: false,
        };
    }

    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    if let Some(index) = fuzzy_content.find(&fuzzy_old_text) {
        return FuzzyMatch {
            found: true,
            index,
            match_length: fuzzy_old_text.len(),
            used_fuzzy_match: true,
        };
    }

    FuzzyMatch {
        found: false,
        index: 0,
        match_length: 0,
        used_fuzzy_match: false,
    }
}

fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    fuzzy_content.match_indices(&fuzzy_old_text).count()
}

fn normalize_for_fuzzy_match(text: &str) -> String {
    let normalized = text.nfkc().collect::<String>();
    normalized
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .map(|ch| match ch {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

fn apply_replacements(content: &str, replacements: &[MatchedEdit], offset: usize) -> String {
    let mut result = content.to_string();
    for replacement in replacements.iter().rev() {
        let match_index = replacement.match_index - offset;
        result.replace_range(
            match_index..match_index + replacement.match_length,
            &replacement.new_text,
        );
    }
    result
}

fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[MatchedEdit],
) -> Result<String, String> {
    let original_lines = split_lines_with_endings(original_content);
    let base_lines = get_line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .to_string(),
        );
    }

    let mut groups: Vec<(usize, usize, Vec<MatchedEdit>)> = Vec::new();
    let mut sorted_replacements = replacements.to_vec();
    sorted_replacements.sort_by_key(|replacement| replacement.match_index);
    for replacement in sorted_replacements {
        let (start_line, end_line) = replacement_line_range(&base_lines, &replacement)?;
        if let Some((_, current_end, current_replacements)) = groups.last_mut()
            && start_line < *current_end
        {
            *current_end = (*current_end).max(end_line);
            current_replacements.push(replacement);
            continue;
        }
        groups.push((start_line, end_line, vec![replacement]));
    }

    let mut original_line_index = 0;
    let mut result = String::new();
    for (start_line, end_line, replacements) in groups {
        result.push_str(&original_lines[original_line_index..start_line].join(""));

        let group_start_offset = base_lines[start_line].start;
        let group_end_offset = base_lines[end_line - 1].end;
        result.push_str(&apply_replacements(
            &base_content[group_start_offset..group_end_offset],
            &replacements,
            group_start_offset,
        ));
        original_line_index = end_line;
    }
    result.push_str(&original_lines[original_line_index..].join(""));
    Ok(result)
}

fn split_lines_with_endings(content: &str) -> Vec<&str> {
    content.split_inclusive('\n').collect()
}

fn get_line_spans(content: &str) -> Vec<LineSpan> {
    let mut offset = 0;
    split_lines_with_endings(content)
        .into_iter()
        .map(|line| {
            let span = LineSpan {
                start: offset,
                end: offset + line.len(),
            };
            offset = span.end;
            span
        })
        .collect()
}

fn replacement_line_range(
    lines: &[LineSpan],
    replacement: &MatchedEdit,
) -> Result<(usize, usize), String> {
    let replacement_start = replacement.match_index;
    let replacement_end = replacement.match_index + replacement.match_length;
    let start_line = lines
        .iter()
        .position(|line| replacement_start >= line.start && replacement_start < line.end)
        .ok_or_else(|| "Replacement range is outside the base content.".to_string())?;
    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].end < replacement_end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        return Err("Replacement range is outside the base content.".to_string());
    }
    Ok((start_line, end_line + 1))
}

fn detect_line_ending(content: &str) -> &'static str {
    if let Some(index) = content.find('\n')
        && index > 0
        && content.as_bytes()[index - 1] == b'\r'
    {
        return "\r\n";
    }
    "\n"
}

fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

fn strip_bom(content: &str) -> (&'static str, &str) {
    content
        .strip_prefix('\u{feff}')
        .map(|text| ("\u{feff}", text))
        .unwrap_or(("", content))
}

fn first_changed_line(old: &str, new: &str) -> Option<usize> {
    let mut old_lines = old.split('\n');
    let mut new_lines = new.split('\n');
    let mut line = 1;
    loop {
        match (old_lines.next(), new_lines.next()) {
            (Some(old_line), Some(new_line)) if old_line == new_line => line += 1,
            (Some(_), Some(_)) | (Some(_), None) | (None, Some(_)) => return Some(line),
            (None, None) => return None,
        }
    }
}

fn not_found_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        )
    } else {
        format!(
            "Could not find edits[{edit_index}] in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    }
}

fn duplicate_error(
    path: &str,
    edit_index: usize,
    total_edits: usize,
    occurrences: usize,
) -> String {
    if total_edits == 1 {
        format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        )
    } else {
        format!(
            "Found {occurrences} occurrences of edits[{edit_index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
        )
    }
}

fn empty_old_text_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!("oldText must not be empty in {path}.")
    } else {
        format!("edits[{edit_index}].oldText must not be empty in {path}.")
    }
}

fn no_change_error(path: &str, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        )
    } else {
        format!("No changes made to {path}. The replacements produced identical content.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn replacement(old_text: impl Into<String>, new_text: impl Into<String>) -> EditReplacement {
        EditReplacement {
            old_text: old_text.into(),
            new_text: new_text.into(),
        }
    }

    fn run_edit(dir: &TempDir, path: &str, edits: Vec<EditReplacement>) -> ToolResult {
        let path = dir.path().join(path).to_string_lossy().to_string();
        edit_file(EditArgs { path, edits })
    }

    #[test]
    fn edit_contract_documents_pi_shape() {
        let definition = edit_tool_definition();
        let rendered = definition.compact_contract().render_signature();

        let schema = serde_json::to_string(&definition.contract.input_schema.canonical).unwrap();
        assert!(schema.contains("oldText"), "{schema}");
        assert!(schema.contains("newText"), "{schema}");
        assert!(rendered.contains("firstChangedLine"), "{rendered}");
        assert!(
            definition
                .manifest()
                .description
                .contains("exact text replacement")
        );
    }

    #[test]
    fn edit_contract_rejects_malformed_edit_item() {
        let definition = edit_tool_definition();

        let error = lash_core::facade_support::validate_tool_input(
            &definition.contract,
            &json!({
                "path": "src/main.rs",
                "edits": [{
                    "oldText": "old();",
                    "newText": "new();",
                    "unexpected": true
                }]
            }),
        )
        .unwrap_err();

        assert!(
            error.contains("Additional properties are not allowed"),
            "{error}"
        );
    }

    #[test]
    fn edit_replaces_one_unique_block() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {\n    old();\n}\n").unwrap();

        let result = run_edit(&dir, "main.rs", vec![replacement("old();", "new();")]);

        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("main.rs")).unwrap(),
            "fn main() {\n    new();\n}\n"
        );
        let value = result.value_for_projection();
        assert!(
            value["summary"]
                .as_str()
                .unwrap()
                .contains("Successfully replaced 1 block(s)")
        );
        assert_eq!(value["details"]["firstChangedLine"], json!(2));
        assert!(
            value["details"]["patch"]
                .as_str()
                .unwrap()
                .contains("-    old();")
        );
    }

    #[test]
    fn edit_replaces_multiple_disjoint_blocks_against_original_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "alpha\nbeta\ngamma\n").unwrap();

        let result = run_edit(
            &dir,
            "notes.txt",
            vec![
                replacement("alpha\n", "ALPHA\n"),
                replacement("gamma\n", "GAMMA\n"),
            ],
        );

        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
            "ALPHA\nbeta\nGAMMA\n"
        );
        assert_eq!(result.value_for_projection()["replacements"], json!(2));
    }

    #[test]
    fn edit_rejects_empty_edit_list() {
        let result = edit_file(EditArgs {
            path: "missing.txt".to_string(),
            edits: Vec::new(),
        });

        assert!(!result.is_success());
        assert!(
            result
                .value_for_projection()
                .to_string()
                .contains("edits must contain at least one replacement")
        );
    }

    #[test]
    fn edit_rejects_empty_old_text() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();

        let result = run_edit(&dir, "a.txt", vec![replacement("", "x")]);

        assert!(!result.is_success());
        assert!(
            result
                .value_for_projection()
                .to_string()
                .contains("oldText must not be empty")
        );
    }

    #[test]
    fn edit_rejects_missing_file() {
        let dir = TempDir::new().unwrap();

        let result = run_edit(&dir, "missing.txt", vec![replacement("a", "b")]);

        let lash_core::ToolCallOutcome::Failure(failure) = &result.as_output().outcome else {
            panic!("missing edit target must fail");
        };
        assert_eq!(failure.class, lash_core::ToolFailureClass::Io);
        assert_eq!(failure.code, "file_metadata_failed");
        assert!(failure.message.contains("missing.txt"));
        assert_eq!(failure.retry, lash_core::ToolRetryDisposition::Never);
    }

    #[test]
    fn edit_rejects_duplicate_matches() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("dup.txt"), "same\nsame\n").unwrap();

        let result = run_edit(&dir, "dup.txt", vec![replacement("same\n", "other\n")]);

        assert!(!result.is_success());
        assert!(
            result
                .value_for_projection()
                .to_string()
                .contains("Found 2 occurrences")
        );
    }

    #[test]
    fn edit_rejects_overlapping_matches() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("overlap.txt"), "abcdef\n").unwrap();

        let result = run_edit(
            &dir,
            "overlap.txt",
            vec![replacement("abc", "ABC"), replacement("bcd", "BCD")],
        );

        assert!(!result.is_success());
        assert!(
            result
                .value_for_projection()
                .to_string()
                .contains("overlap")
        );
    }

    #[test]
    fn edit_does_not_match_second_edit_against_first_replacement() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("original.txt"), "alpha\n").unwrap();

        let result = run_edit(
            &dir,
            "original.txt",
            vec![replacement("alpha", "beta"), replacement("beta", "gamma")],
        );

        assert!(!result.is_success());
        assert!(
            result
                .value_for_projection()
                .to_string()
                .contains("Could not find edits[1]")
        );
    }

    #[test]
    fn edit_preserves_crlf_and_bom() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("windows.txt"),
            "\u{feff}first\r\nsecond\r\nthird\r\n",
        )
        .unwrap();

        let result = run_edit(
            &dir,
            "windows.txt",
            vec![replacement("second\n", "SECOND\n")],
        );

        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("windows.txt")).unwrap(),
            "\u{feff}first\r\nSECOND\r\nthird\r\n"
        );
    }

    #[test]
    fn edit_fuzzy_matches_common_unicode_and_trailing_whitespace() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("unicode.txt"),
            "before\nquote \u{201C}value\u{201D} uses dash \u{2013} and space\u{00A0}   \nafter\n",
        )
        .unwrap();

        let result = run_edit(
            &dir,
            "unicode.txt",
            vec![replacement(
                "quote \"value\" uses dash - and space ",
                "normalized line",
            )],
        );

        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("unicode.txt")).unwrap(),
            "before\nnormalized line\nafter\n"
        );
    }

    #[test]
    fn edit_fuzzy_matching_preserves_untouched_lines() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("preserve.txt"),
            "keep \u{201C}smart\u{201D}\nchange \u{2013}\nkeep \u{00A0}space\n",
        )
        .unwrap();

        let result = run_edit(
            &dir,
            "preserve.txt",
            vec![replacement("change -", "changed")],
        );

        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("preserve.txt")).unwrap(),
            "keep \u{201C}smart\u{201D}\nchanged\nkeep \u{00A0}space\n"
        );
    }

    #[test]
    fn edit_rejects_no_change_replacement() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("same.txt"), "alpha\n").unwrap();

        let result = run_edit(&dir, "same.txt", vec![replacement("alpha", "alpha")]);

        assert!(!result.is_success());
        assert!(
            result
                .value_for_projection()
                .to_string()
                .contains("No changes")
        );
    }
}
