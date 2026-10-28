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
    append(arena, from, part.len() as u64, memory, free, |arena| {
        arena.extend_text(from, part, |grown| !memory || grown <= free)
    })
}

#[inline(always)]
pub(in crate::machine::islands) fn append_long_text(
    arena: &mut Arena,
    from: ArenaHandle,
    right: ArenaHandle,
    memory: bool,
    free: u64,
) -> Option<Option<Appended>> {
    let part_bytes = match arena.body(right).ok()? {
        lash_kernel_doc::arena::Body::LongText(text) => match &*text.chunks {
            [lash_kernel_doc::arena::Chunk::Resident(part)] => part.len() as u64,
            _ => return None,
        },
        _ => return None,
    };
    append(arena, from, part_bytes, memory, free, |arena| {
        arena.extend_text_from(from, right, |grown| !memory || grown <= free)
    })
}

#[inline(always)]
fn append(
    arena: &mut Arena,
    from: ArenaHandle,
    part_bytes: u64,
    memory: bool,
    free: u64,
    extend: impl FnOnce(&mut Arena) -> Option<(ArenaHandle, usize)>,
) -> Option<Option<Appended>> {
    let bytes = match arena.body(from).ok()? {
        lash_kernel_doc::arena::Body::LongText(text) => text.summary.utf8_bytes,
        _ => return None,
    };
    let len = bytes.checked_add(part_bytes)?;
    let held = if memory {
        let bound = arena.text_install_bound(len).checked_add(len)?;
        if bound > free {
            return Some(None);
        }
        len
    } else {
        0
    };
    let (text, moved) = extend(arena)?;
    Some(Some(Appended {
        made: Made {
            text,
            moved: Some(moved),
        },
        held,
        bytes,
    }))
}
