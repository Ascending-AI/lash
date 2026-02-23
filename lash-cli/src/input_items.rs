use std::collections::HashMap;
use std::path::PathBuf;

use crate::app::App;
use lash_core::InputItem;

/// Build structured turn items from editor input:
/// - `@path` becomes `FileRef` or `DirRef` when resolvable
/// - `[Image #n]` binds to pasted image `n` from this turn's image list
/// - plain text remains `Text`
pub fn build_items_from_editor_input(
    input: &str,
    images: Vec<Vec<u8>>,
) -> (Vec<InputItem>, HashMap<String, Vec<u8>>) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut items: Vec<InputItem> = Vec::new();
    let mut image_blobs: HashMap<String, Vec<u8>> = HashMap::new();
    let mut image_slots: Vec<Option<(String, Vec<u8>)>> = images
        .into_iter()
        .enumerate()
        .map(|(i, bytes)| (format!("img-{}", i + 1), bytes))
        .map(Some)
        .collect();

    let mut text_buf = String::with_capacity(input.len());
    let mut i = 0;
    let bytes = input.as_bytes();

    while i < bytes.len() {
        if let Some((next_i, img_idx)) = parse_image_marker_at(input, i)
            && let Some(slot) = image_slots
                .get_mut(img_idx.saturating_sub(1))
                .and_then(Option::take)
        {
            push_text_item(&mut items, &mut text_buf);
            let (id, data) = slot;
            image_blobs.insert(id.clone(), data);
            items.push(InputItem::ImageRef { id });
            i = next_i;
            continue;
        }

        if bytes[i] == b'@' {
            // Check: must be at start or preceded by whitespace
            let at_start = i == 0 || bytes[i - 1].is_ascii_whitespace();
            if at_start {
                // Find the end of the token (next whitespace or end)
                let token_start = i + 1;
                let mut token_end = token_start;
                while token_end < bytes.len() && !bytes[token_end].is_ascii_whitespace() {
                    token_end += 1;
                }
                let token = &input[token_start..token_end];
                if !token.is_empty() {
                    let path = if token.starts_with('/') {
                        PathBuf::from(token)
                    } else {
                        cwd.join(token)
                    };
                    if path.is_file() {
                        push_text_item(&mut items, &mut text_buf);
                        items.push(InputItem::FileRef {
                            path: path.display().to_string(),
                        });
                        i = token_end;
                        continue;
                    } else if path.is_dir() {
                        push_text_item(&mut items, &mut text_buf);
                        items.push(InputItem::DirRef {
                            path: path.display().to_string(),
                        });
                        i = token_end;
                        continue;
                    }
                }
            }
        }
        let ch = input[i..].chars().next().unwrap();
        text_buf.push(ch);
        i += ch.len_utf8();
    }

    push_text_item(&mut items, &mut text_buf);

    // Preserve any pasted images even if their inline markers were removed.
    for slot in image_slots.into_iter().flatten() {
        let (id, data) = slot;
        image_blobs.insert(id.clone(), data);
        items.push(InputItem::ImageRef { id });
    }

    (items, image_blobs)
}

fn push_text_item(items: &mut Vec<InputItem>, text: &mut String) {
    if text.is_empty() {
        return;
    }
    if let Some(InputItem::Text { text: prev }) = items.last_mut() {
        prev.push_str(text);
        text.clear();
        return;
    }
    items.push(InputItem::Text {
        text: std::mem::take(text),
    });
}

pub(crate) fn parse_image_marker_at(input: &str, start: usize) -> Option<(usize, usize)> {
    let rest = &input[start..];
    let prefix = "[Image #";
    if !rest.starts_with(prefix) {
        return None;
    }
    let after = &rest[prefix.len()..];
    let digits_len = after
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .map(char::len_utf8)
        .sum::<usize>();
    if digits_len == 0 {
        return None;
    }
    let digits = &after[..digits_len];
    let remaining = &after[digits_len..];
    if !remaining.starts_with(']') {
        return None;
    }
    let idx = digits.parse::<usize>().ok()?;
    if idx == 0 {
        return None;
    }
    Some((start + prefix.len() + digits_len + 1, idx))
}

/// Insert an inline attachment marker like `[Image #1]` at the current cursor,
/// adding surrounding spaces when needed so it reads naturally in the input.
pub fn insert_inline_marker(app: &mut App, marker: &str) {
    let needs_leading_space = app.cursor_pos > 0
        && app.input[..app.cursor_pos]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace());

    let needs_trailing_space = app.cursor_pos < app.input.len()
        && app.input[app.cursor_pos..]
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace());

    if needs_leading_space {
        app.insert_char(' ');
    }
    for ch in marker.chars() {
        app.insert_char(ch);
    }
    if needs_trailing_space {
        app.insert_char(' ');
    }
}
