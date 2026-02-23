use std::fmt::Write;

/// Hashline hashes are emitted as fixed-width lowercase hex.
const HASH_HEX_LEN: usize = 8;

/// Compute an 8-char hex hash for a line.
/// Uses full xxHash32 of whitespace-stripped content.
pub fn compute_line_hash(line: &str) -> String {
    let mut hasher = xxhash_rust::xxh32::Xxh32::new(0);
    for &b in line.as_bytes() {
        if !b.is_ascii_whitespace() {
            hasher.update(&[b]);
        }
    }
    let hash = hasher.digest();
    format!("{:08x}", hash)
}

/// Format file content with hashline prefixes.
/// Each line becomes `{line_num}:{hash}|{content}`.
pub fn format_hashlines(content: &str, start_line: usize) -> String {
    let mut out = String::new();
    for (i, line) in content.lines().enumerate() {
        let line_num = start_line + i;
        let hash = compute_line_hash(line);
        let _ = writeln!(out, "{}:{}|{}", line_num, hash, line);
    }
    // If content ends without a trailing newline and is non-empty,
    // our writeln already added one. If content is empty, return empty.
    if out.ends_with('\n') && !content.is_empty() {
        out.truncate(out.len() - 1);
    }
    out
}

/// Parse an anchor like "42:a5c1d2e3" into (line_number, hash).
pub fn parse_anchor(anchor: &str) -> Result<(usize, String), String> {
    let parts: Vec<&str> = anchor.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Invalid anchor format '{}', expected LINE:HASH",
            anchor
        ));
    }
    let line_num: usize = parts[0]
        .parse()
        .map_err(|_| format!("Invalid line number in anchor '{}'", anchor))?;
    let hash = parts[1];
    if hash.len() != HASH_HEX_LEN || !hash.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "Invalid hash in anchor '{}', expected {} hex chars (e.g. 42:a5c1d2e3)",
            anchor, HASH_HEX_LEN
        ));
    }
    Ok((line_num, hash.to_ascii_lowercase()))
}

/// Strip accidental `LINE:HASH|` prefixes from replacement text.
pub fn strip_hashline_prefix(text: &str) -> String {
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        // Match pattern: digits:8hex|rest
        if let Some(rest) = try_strip_prefix(line) {
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
    }
    out
}

fn try_strip_prefix(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut i = 0;
    // Skip digits
    if i >= bytes.len() || !bytes[i].is_ascii_digit() {
        return None;
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // Expect ':'
    if i >= bytes.len() || bytes[i] != b':' {
        return None;
    }
    i += 1;
    // Expect exactly HASH_HEX_LEN hex chars
    if i + HASH_HEX_LEN > bytes.len() {
        return None;
    }
    if !bytes[i..i + HASH_HEX_LEN]
        .iter()
        .all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    i += HASH_HEX_LEN;
    // Expect '|'
    if i >= bytes.len() || bytes[i] != b'|' {
        return None;
    }
    i += 1;
    Some(&line[i..])
}

/// A single hashline edit operation.
#[derive(Debug, Clone)]
pub enum HashlineEdit {
    /// Replace a single line identified by anchor.
    SetLine { anchor: String, new_text: String },
    /// Replace a range of lines (inclusive).
    ReplaceLines {
        start_anchor: String,
        end_anchor: String,
        new_text: String,
    },
    /// Insert text after the anchor line.
    InsertAfter { anchor: String, text: String },
    /// Find-and-replace text (fallback, no anchors).
    Replace {
        old_text: String,
        new_text: String,
        all: bool,
    },
}

/// Validate an anchor against file lines. Returns the 0-indexed line index.
fn validate_anchor(lines: &[&str], anchor: &str) -> Result<usize, String> {
    let (line_num, expected_hash) = parse_anchor(anchor)?;
    // line_num is 1-based
    if line_num == 0 || line_num > lines.len() {
        return Err(format!(
            "Line {} out of range (file has {} lines)",
            line_num,
            lines.len()
        ));
    }
    let idx = line_num - 1;
    let actual_hash = compute_line_hash(lines[idx]);
    if actual_hash != expected_hash {
        Err(format!(
            "Hash mismatch at line {}: expected '{}', actual '{}' (content: {:?})",
            line_num,
            expected_hash,
            actual_hash,
            truncate_str(lines[idx], 80),
        ))
    } else {
        Ok(idx)
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

/// Represents a resolved splice operation on lines.
struct Splice {
    start: usize, // inclusive 0-based index
    end: usize,   // inclusive 0-based index
    replacement: Vec<String>,
}

/// Apply hashline edits to file content.
/// Pre-validates all anchors, then applies edits bottom-up.
pub fn apply_hashline_edits(content: &str, edits: Vec<HashlineEdit>) -> Result<String, String> {
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    // Handle trailing newline: if content ends with \n, lines() won't include an empty trailing element
    // We need to preserve the original trailing newline behavior

    let mut splices: Vec<Splice> = Vec::new();
    let mut text_replaces: Vec<(String, String, bool)> = Vec::new();

    // First pass: validate all anchors and collect splices
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();

    for edit in &edits {
        match edit {
            HashlineEdit::SetLine { anchor, new_text } => {
                let idx = validate_anchor(&line_refs, anchor)?;
                let cleaned = strip_hashline_prefix(new_text);
                splices.push(Splice {
                    start: idx,
                    end: idx,
                    replacement: cleaned.lines().map(|l| l.to_string()).collect(),
                });
            }
            HashlineEdit::ReplaceLines {
                start_anchor,
                end_anchor,
                new_text,
            } => {
                let start = validate_anchor(&line_refs, start_anchor)?;
                let end = validate_anchor(&line_refs, end_anchor)?;
                if end < start {
                    return Err(format!(
                        "End anchor {} is before start anchor {}",
                        end_anchor, start_anchor
                    ));
                }
                let cleaned = strip_hashline_prefix(new_text);
                splices.push(Splice {
                    start,
                    end,
                    replacement: if cleaned.is_empty() {
                        vec![]
                    } else {
                        cleaned.lines().map(|l| l.to_string()).collect()
                    },
                });
            }
            HashlineEdit::InsertAfter { anchor, text } => {
                let idx = validate_anchor(&line_refs, anchor)?;
                let cleaned = strip_hashline_prefix(text);
                // Insert after = replace range (idx+1..idx) which inserts at idx+1
                splices.push(Splice {
                    start: idx + 1,
                    end: idx, // end < start means pure insertion
                    replacement: cleaned.lines().map(|l| l.to_string()).collect(),
                });
            }
            HashlineEdit::Replace {
                old_text,
                new_text,
                all,
            } => {
                text_replaces.push((old_text.clone(), new_text.clone(), *all));
            }
        }
    }

    // Sort splices bottom-up (highest start first) to preserve indices
    splices.sort_by(|a, b| b.start.cmp(&a.start));

    // Check for overlapping splices
    for i in 0..splices.len().saturating_sub(1) {
        let higher = &splices[i];
        let lower = &splices[i + 1];
        // higher.start >= lower.start (sorted descending)
        if lower.end >= higher.start && lower.start <= higher.end {
            return Err(format!(
                "Overlapping edits: lines {}-{} and {}-{}",
                lower.start + 1,
                lower.end + 1,
                higher.start + 1,
                higher.end + 1,
            ));
        }
    }

    // Apply splices bottom-up
    for splice in splices {
        if splice.end < splice.start {
            // Pure insertion at splice.start
            for (i, line) in splice.replacement.into_iter().enumerate() {
                let insert_at = splice.start.min(lines.len());
                lines.insert(insert_at + i, line);
            }
        } else {
            // Remove the range and insert replacement
            let drain_end = (splice.end + 1).min(lines.len());
            lines.drain(splice.start..drain_end);
            for (i, line) in splice.replacement.into_iter().enumerate() {
                lines.insert(splice.start + i, line);
            }
        }
    }

    // Apply text replaces on the joined content
    let mut result = lines.join("\n");
    // Preserve trailing newline if original had one
    if content.ends_with('\n') {
        result.push('\n');
    }

    for (old_text, new_text, all) in text_replaces {
        if all {
            result = result.replace(&old_text, &new_text);
        } else {
            result = result.replacen(&old_text, &new_text, 1);
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_hash() {
        let hash = compute_line_hash("  hello world  ");
        // Should hash "helloworld"
        let hash2 = compute_line_hash("helloworld");
        assert_eq!(hash, hash2);
        assert_eq!(hash.len(), HASH_HEX_LEN);
    }

    #[test]
    fn test_format_hashlines() {
        let content = "fn main() {\n    println!(\"hello\");\n}";
        let result = format_hashlines(content, 1);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("1:"));
        assert!(lines[0].contains("|fn main() {"));
        assert!(lines[1].starts_with("2:"));
        assert!(lines[2].starts_with("3:"));
    }

    #[test]
    fn test_parse_anchor() {
        let (line, hash) = parse_anchor("42:a5c1d2e3").unwrap();
        assert_eq!(line, 42);
        assert_eq!(hash, "a5c1d2e3");
    }

    #[test]
    fn test_strip_hashline_prefix() {
        assert_eq!(strip_hashline_prefix("1:a5c1d2e3|hello"), "hello");
        assert_eq!(strip_hashline_prefix("no prefix"), "no prefix");
        assert_eq!(
            strip_hashline_prefix("1:a5c1d2e3|line1\n2:0fedcba9|line2"),
            "line1\nline2"
        );
    }

    #[test]
    fn test_set_line() {
        let content = "line1\nline2\nline3\n";
        let hash2 = compute_line_hash("line2");
        let edits = vec![HashlineEdit::SetLine {
            anchor: format!("2:{}", hash2),
            new_text: "replaced".into(),
        }];
        let result = apply_hashline_edits(content, edits).unwrap();
        assert_eq!(result, "line1\nreplaced\nline3\n");
    }

    #[test]
    fn test_insert_after() {
        let content = "line1\nline2\nline3";
        let hash1 = compute_line_hash("line1");
        let edits = vec![HashlineEdit::InsertAfter {
            anchor: format!("1:{}", hash1),
            text: "inserted".into(),
        }];
        let result = apply_hashline_edits(content, edits).unwrap();
        assert_eq!(result, "line1\ninserted\nline2\nline3");
    }

    #[test]
    fn test_replace_lines() {
        let content = "a\nb\nc\nd\ne";
        let hash_b = compute_line_hash("b");
        let hash_d = compute_line_hash("d");
        let edits = vec![HashlineEdit::ReplaceLines {
            start_anchor: format!("2:{}", hash_b),
            end_anchor: format!("4:{}", hash_d),
            new_text: "X\nY".into(),
        }];
        let result = apply_hashline_edits(content, edits).unwrap();
        assert_eq!(result, "a\nX\nY\ne");
    }

    #[test]
    fn test_hash_mismatch() {
        let content = "line1\nline2\nline3";
        let edits = vec![HashlineEdit::SetLine {
            anchor: "2:ffffffff".into(),
            new_text: "replaced".into(),
        }];
        let result = apply_hashline_edits(content, edits);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Hash mismatch"));
    }

    // ── Additional edge cases ──

    #[test]
    fn test_strip_hashline_prefix_mixed() {
        let input = "1:a5c1d2e3|prefixed line\nnormal line\n42:ffffffff|another prefixed";
        let result = strip_hashline_prefix(input);
        assert_eq!(result, "prefixed line\nnormal line\nanother prefixed");
    }

    #[test]
    fn test_empty_edits_unchanged() {
        let content = "line1\nline2\nline3";
        let result = apply_hashline_edits(content, vec![]).unwrap();
        assert_eq!(result, content);
    }

    #[test]
    fn test_insert_after_last_line() {
        let content = "line1\nline2\nline3";
        let hash3 = compute_line_hash("line3");
        let edits = vec![HashlineEdit::InsertAfter {
            anchor: format!("3:{}", hash3),
            text: "line4\nline5".into(),
        }];
        let result = apply_hashline_edits(content, edits).unwrap();
        assert_eq!(result, "line1\nline2\nline3\nline4\nline5");
    }

    #[test]
    fn test_overlapping_ranges_error() {
        let content = "a\nb\nc\nd\ne";
        let hash_a = compute_line_hash("a");
        let hash_c = compute_line_hash("c");
        let hash_b = compute_line_hash("b");
        let hash_d = compute_line_hash("d");
        let edits = vec![
            HashlineEdit::ReplaceLines {
                start_anchor: format!("1:{}", hash_a),
                end_anchor: format!("3:{}", hash_c),
                new_text: "X".into(),
            },
            HashlineEdit::ReplaceLines {
                start_anchor: format!("2:{}", hash_b),
                end_anchor: format!("4:{}", hash_d),
                new_text: "Y".into(),
            },
        ];
        let result = apply_hashline_edits(content, edits);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Overlapping"));
    }

    #[test]
    fn test_validate_anchor_hash_mismatch_message() {
        let lines = vec!["hello"];
        let result = validate_anchor(&lines, "1:ffffffff");
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("Hash mismatch at line 1"));
        assert!(msg.contains("expected 'ffffffff'"));
    }

    #[test]
    fn test_validate_anchor_out_of_bounds() {
        let lines = vec!["hello"];
        let result = validate_anchor(&lines, "5:a5c1d2e3");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("out of range"));
    }
}
