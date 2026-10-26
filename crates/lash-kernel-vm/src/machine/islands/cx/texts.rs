//! Owned append admission shared by the heap context and register instruction.

use super::*;

pub(in crate::machine::islands) struct Appended {
    pub(in crate::machine::islands) made: Made,
    pub(in crate::machine::islands) held: u64,
    pub(in crate::machine::islands) bytes: u64,
}

/// The same immutable result and native work reservation as text.concat.
/// Refusal changes no text; a moved result carries the obligation to give
/// its storage back if its statement stops before writing it.
#[inline(always)]
pub(in crate::machine::islands) fn append_text(
    arena: &mut Arena,
    from: ArenaHandle,
    part: &str,
    memory: bool,
    free: u64,
) -> Option<Option<Appended>> {
    let bytes = match arena.body(from).ok()? {
        lash_kernel_doc::arena::Body::LongText(text) => text.summary.utf8_bytes,
        _ => return None,
    };
    let len = bytes.checked_add(part.len() as u64)?;
    let held = if memory {
        let bound = arena.text_install_bound(len).checked_add(len)?;
        if bound > free {
            return Some(None);
        }
        len
    } else {
        0
    };
    let (text, moved) = arena.extend_text(from, part, |grown| !memory || grown <= free)?;
    Some(Some(Appended {
        made: Made {
            text,
            moved: Some(moved),
        },
        held,
        bytes,
    }))
}
