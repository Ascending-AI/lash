//! The occupancy rule of a mutable container's index (lead ruling, l65f
//! item 1): its node widths may depend on its history, but every index
//! page except the last of its level names at least half of
//! [`NODE_REFS`] references, so an index at the least height its leaves
//! give holds no more pages than about twice a direct build's.

use lash_kernel_state::kernel_internals::tree::grant;
use lash_kernel_state::paged::run::index::{Leaf, NODE_REFS, Part};
use lash_kernel_value::tokens::decode_body;

use super::*;

/// The references each index page of `index` names, read cold: a level
/// to a row, from the pages the top names down, each left to right.
pub(super) fn widths<L: Leaf>(
    store: &Counting,
    index: &Index<L>,
) -> Result<Vec<Vec<usize>>, String> {
    let access = Access::new(Box::new(store.clone()));
    let Index::Nodes { height, refs } = index else {
        return Ok(Vec::new());
    };
    let mut pages: Vec<PageRef> = refs.iter().map(|part| part.page).collect();
    let mut levels = Vec::new();
    for height in (1..=*height).rev() {
        let mut level = Vec::with_capacity(pages.len());
        let mut below = Vec::new();
        for page in pages {
            let answer = access.read(page).map_err(|error| format!("{error:?}"))?;
            if height == 1 {
                let leaves: Vec<L> =
                    decode_body(&answer.tokens).map_err(|error| error.to_string())?;
                level.push(leaves.len());
            } else {
                let parts: Vec<Part<L::Key>> =
                    decode_body(&answer.tokens).map_err(|error| error.to_string())?;
                level.push(parts.len());
                for mut part in parts {
                    grant(&mut part.page, &answer.children)
                        .map_err(|error| format!("{error:?}"))?;
                    below.push(part.page);
                }
            }
        }
        levels.push(level);
        pages = below;
    }
    Ok(levels)
}

/// The occupancy rule: every index page of `index` but the last of its
/// level names at least half of [`NODE_REFS`] references.
pub(super) fn occupied<L: Leaf>(store: &Counting, index: &Index<L>) -> Result<(), String> {
    for (depth, level) in widths(store, index)?.iter().enumerate() {
        let last = level.len().saturating_sub(1);
        if let Some((at, width)) = level
            .iter()
            .enumerate()
            .find(|(at, width)| *at < last && **width < NODE_REFS / 2)
        {
            return Err(format!(
                "index page {at} of {} at level {depth} below the top names {width} references \
                 (the level: {level:?})",
                level.len()
            ));
        }
    }
    Ok(())
}

/// The thinned case: a map of 720 chunks (ten index pages of 72) whose
/// chunks are emptied, but the first of each index page and the whole of
/// the last page, sealed in one edit, reopens cold holding the rest, its
/// row [`well_formed`]: 81 chunks at height one, its index pages occupied.
pub(super) fn table_thinned_case() -> Result<(), String> {
    let n: i64 = 720 * 48;
    let store = Counting::default();
    let mut heap = fresh(&store);
    let table = make(
        &mut heap,
        NewObject::Map((0..n).map(|i| (Value::Int(i), Value::Int(-i))).collect()),
    );
    let root = heap.seal(&[table]).map_err(|error| format!("{error:?}"))?;
    let built = row(&store, &root)?;
    let pages = widths(&store, &built.index)?
        .pop()
        .ok_or("the chunk index names no page")?;
    if pages.len() < 3 {
        return Err(format!("the map's index pages are {pages:?}"));
    }
    let access = Access::new(Box::new(store.clone()));
    let mut chunks = Vec::new();
    built
        .index
        .each(&access, &built.shape(), &mut |_, chunk| {
            chunks.push(*chunk);
            Ok(false)
        })
        .map_err(|error| format!("{error:?}"))?;
    let mut emptied = Vec::new();
    let mut start = 0;
    for (at, width) in pages.iter().enumerate() {
        if at + 1 < pages.len() {
            for chunk in &chunks[start + 1..start + width] {
                let (page, _) = chunk_page(&access, chunk).map_err(|error| format!("{error:?}"))?;
                let Members::Table(entries) = page.members else {
                    return Err("a map's chunk holds a list's members".into());
                };
                emptied.extend(entries.iter().filter_map(|entry| match &entry.key {
                    RunValue::Int(i) => Some(*i),
                    _ => None,
                }));
            }
        }
        start += width;
    }
    let loads = usize::try_from(2 * n / 32 + 64).expect("a bound");
    let (mut heap, table) = reopened(&store, &root);
    let handle = table.handle().ok_or("no map")?;
    for i in &emptied {
        let gone = Value::Int(*i);
        warm_within(&mut heap, &[table], loads, |heap| {
            heap.arena_mut().table_delete(handle, &gone)
        });
    }
    let root = heap.seal(&[table]).map_err(|error| format!("{error:?}"))?;
    let (mut cold, table) = reopened(&store, &root);
    let handle = table.handle().ok_or("no map")?;
    let held = warm_within(&mut cold, &[table], loads, |heap| heap.arena().len(handle));
    let kept = n as u64 - emptied.len() as u64;
    if held != kept {
        return Err(format!("thinned: {held} entries, not {kept}"));
    }
    let kept_key = (0..n)
        .find(|i| !emptied.contains(i))
        .ok_or("every entry was emptied")?;
    if int(get(&mut cold, &[table], table, Value::Int(kept_key))) != Some(-kept_key) {
        return Err(format!("thinned: entry {kept_key} reads otherwise"));
    }
    let thinned = row(&store, &root)?;
    well_formed(&store, &thinned).map_err(|error| format!("thinned: {error}"))
}

/// The thinned case at the index itself, at height 2: a filled index of
/// 20,736 leaves (288 pages of 72 under 4 of 72) loses, in one edit, all
/// but the first leaf of its first upper page, so that page holds one
/// page of one leaf; 30 leaves of the last page under its third, which
/// thins to 42 at the end of a page that is not its level's last; and 25
/// of a page of its last, which thins to 47 beside a kept page of 72. It
/// is read back cold whole, at the height its leaves give, its pages
/// occupied.
pub(super) fn index_thinned_case() -> Result<(), String> {
    use std::collections::BTreeSet;

    use lash_kernel_state::paged::PageKind;
    use lash_kernel_state::paged::run::continuation::{SEGMENTS, Segment};
    use lash_kernel_state::paged::run::index::Shape;
    let shape = Shape {
        full: false,
        ..SEGMENTS
    };
    let n = 288 * 72u64;
    let store = Counting::default();
    let access = Access::new(Box::new(store.clone()));
    let leaves: Vec<Segment> = (0..n)
        .map(|i| {
            access
                .seal(PageKind::RUN_SEGMENT, i.to_le_bytes().to_vec(), Vec::new())
                .map(Segment)
        })
        .collect::<Result<_, _>>()
        .map_err(|error| format!("{error:?}"))?;
    let index =
        Index::build(&access, &shape, leaves.clone()).map_err(|error| format!("{error:?}"))?;
    if index.height() != 2 || index.width() != 4 {
        return Err(format!(
            "built at height {} under {} pages",
            index.height(),
            index.width()
        ));
    }
    let page = |at: u64| at * 72..(at + 1) * 72;
    let mut dropped: BTreeSet<u64> = (1..72 * 72).collect();
    dropped.extend(page(215).skip(42));
    dropped.extend(page(221).skip(47));
    let index = index
        .rebuild(
            &access,
            &shape,
            &dropped,
            &mut |_, _| Ok(Vec::new()),
            Vec::new(),
        )
        .map_err(|error| format!("{error:?}"))?;
    index.check(&shape)?;
    let kept: Vec<Segment> = (0..n)
        .filter(|at| !dropped.contains(at))
        .map(|at| leaves[at as usize])
        .collect();
    let cold = Access::new(Box::new(store.clone()));
    let mut held = Vec::new();
    index
        .each(&cold, &shape, &mut |_, leaf| {
            held.push(*leaf);
            Ok(false)
        })
        .map_err(|error| format!("{error:?}"))?;
    if held != kept || index.height() != shape.height(kept.len() as u64) {
        return Err(format!(
            "{} leaves at height {}, not {} at {}",
            held.len(),
            index.height(),
            kept.len(),
            shape.height(kept.len() as u64)
        ));
    }
    occupied(&store, &index)
}
