//! The parent's library reservations, read without per-body census work.
use super::lazy::{Body, LibraryTables, NO_BODY, census_of};
use super::*;
use std::sync::OnceLock;

impl PreparedLibrary {
    /// Every body's reservation in registry order, with the totals first.
    #[must_use]
    pub fn frame_index(registry: &FunctionRegistry) -> Vec<u8> {
        let mut bodies = Vec::new();
        let mut totals = [0u32; 4];
        let mut occurrences = Vec::new();
        for (at, (_, registered)) in registry.iter().enumerate() {
            let Some(census) = census_of(registered, registry) else {
                continue;
            };
            bodies.extend(
                [
                    at as u32,
                    totals[0],
                    totals[1],
                    totals[2],
                    totals[3],
                    census.codes,
                    census.blocks,
                    census.stmts,
                    census.sites,
                    u32::from(census.closures),
                ]
                .iter()
                .flat_map(|word| word.to_le_bytes()),
            );
            totals[0] += census.codes;
            totals[1] += census.blocks;
            totals[2] += census.stmts;
            totals[3] += census.sites;
            for site in &census.occurrences {
                let encoded = lash_vocabulary::tokens::encode_body(site);
                occurrences.extend((encoded.len() as u32).to_le_bytes());
                occurrences.extend(encoded);
            }
        }
        let mut out = Vec::new();
        for word in [
            registry.len() as u32,
            (bodies.len() / 40) as u32,
            totals[0],
            totals[1],
            totals[2],
            totals[3],
            occurrences.len() as u32,
        ] {
            out.extend(word.to_le_bytes());
        }
        out.extend(bodies);
        out.extend(occurrences);
        out
    }

    /// The identity layout over `registry`, its reservations read from
    /// `index` ([`PreparedLibrary::frame_index`] over the same functions).
    pub fn over_frame(
        registry: Arc<FunctionRegistry>,
        charge: crate::Charge,
        index: &'static [u8],
    ) -> std::io::Result<Self> {
        if index.len() < 28 {
            return Err(std::io::Error::other("truncated library reservation"));
        }
        let word = |at: usize| {
            u32::from_le_bytes([index[at], index[at + 1], index[at + 2], index[at + 3]])
        };
        let (n, count) = (word(0) as usize, word(4) as usize);
        if n != registry.len()
            || 28usize
                .checked_add(count.saturating_mul(40))
                .and_then(|size| size.checked_add(word(24) as usize))
                != Some(index.len())
        {
            return Err(std::io::Error::other(
                "library reservation does not match the frame",
            ));
        }
        let record = |owner: usize, field: usize| word(28 + owner * 40 + field * 4);
        let owners =
            |base: usize, length: usize, total: usize| -> std::io::Result<doc::FrameArray<usize>> {
                let mut owner = 0;
                doc::FrameArray::mapped(total, |at| {
                    while at >= (record(owner, base) + record(owner, length)) as usize {
                        owner += 1;
                    }
                    owner
                })
            };
        let functions = doc::FrameArray::mapped(n, |at| {
            registry
                .function_at(at)
                .unwrap_or_else(|| unreachable!("frame identity"))
        })?;
        let mut owner = 0;
        let body_of = doc::FrameArray::mapped(n, |at| {
            if owner < count && record(owner, 0) as usize == at {
                let found = owner as u32;
                owner += 1;
                found
            } else {
                NO_BODY
            }
        })?;
        let mut tables = LibraryTables {
            bodies: doc::FrameArray::mapped(count, |owner| Body {
                function: functions[record(owner, 0) as usize],
                base: Base {
                    codes: record(owner, 1),
                    blocks: record(owner, 2),
                    stmts: record(owner, 3),
                    occurrences: 0,
                    sites: record(owner, 4),
                },
                tables: OnceLock::new(),
                closures: record(owner, 9) != 0,
            })?,
            body_of,
            codes: owners(1, 5, word(8) as usize)?,
            blocks: owners(2, 6, word(12) as usize)?,
            stmts: owners(3, 7, word(16) as usize)?,
            ..LibraryTables::default()
        };
        let runs = doc::FrameArray::mapped(n, |at| {
            if tables.body_of[at] == NO_BODY {
                Runs::Answers
            } else {
                Runs::Body
            }
        })?;
        let mut sites = word(20);
        let mut at = 28 + count * 40;
        let end = at + word(24) as usize;
        let mut occurrence_at = BTreeMap::new();
        while at < end {
            let len = word(at) as usize;
            at += 4;
            let site: Site = lash_vocabulary::tokens::decode_body(
                index
                    .get(at..at + len)
                    .ok_or_else(|| std::io::Error::other("truncated occurrence site"))?,
            )
            .map_err(std::io::Error::other)?;
            at += len;
            occurrence_at.insert(site, 0);
        }
        let by_key: BTreeMap<_, _> = occurrence_at
            .into_keys()
            .map(|site| {
                (
                    lash_kernel_state::kernel_internals::metadata::metadata_key(&site),
                    site,
                )
            })
            .collect();
        tables.occurrence_keys = by_key.keys().cloned().collect();
        tables.occurrence_at = by_key
            .into_values()
            .enumerate()
            .map(|(at, site)| (site, at))
            .collect();
        let plan_sites = doc::FrameArray::mapped(n, |at| {
            let start = sites;
            if let Some(plan) = registry.plan_at(at) {
                sites += plan.fields.len() as u32;
            }
            start
        })?;
        let missing = if registry.holds_every_callee() {
            BTreeMap::new()
        } else {
            missing(&registry)
        };
        let libs = doc::FrameArray::mapped(n, |_| OnceLock::new())?;
        Ok(Self(
            Arc::new(Prepared {
                charge,
                alternative: Default::default(),
                registry,
                layout: Layout::default(),
                tables,
                functions,
                lib_at: None,
                libs,
                missing,
                sites,
                documents: Default::default(),
                runs,
                plan_sites,
                frame_index: Some(index),
            }),
            doc::BuildHelpers::empty(),
        ))
    }
}
