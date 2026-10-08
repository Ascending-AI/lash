//! Input projection preserves ordered refs; producers put before constructing input.

use crate::InputItem;

#[derive(Clone, Debug)]
pub enum NormalizedItem {
    Text(String),
    Attachment(crate::AttachmentRef),
}

pub fn normalize_input_items(items: &[InputItem]) -> Vec<NormalizedItem> {
    let mut out = Vec::new();
    for item in items {
        match item {
            InputItem::Text { text } => push_text(&mut out, text.clone()),
            InputItem::Attachment { reference } => {
                out.push(NormalizedItem::Attachment(reference.clone()));
            }
        }
    }
    out
}

fn push_text(out: &mut Vec<NormalizedItem>, text: String) {
    if text.is_empty() {
        return;
    }
    if let Some(NormalizedItem::Text(last)) = out.last_mut() {
        last.push_str(&text);
    } else {
        out.push(NormalizedItem::Text(text));
    }
}
