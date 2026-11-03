//! Owned append admission shared by the heap context and register instruction.

use super::*;

pub(in crate::machine::islands) struct Appended {
    pub(in crate::machine::islands) made: Made,
    pub(in crate::machine::islands) held: u64,
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
    }))
}

/// Actual operands, result and native scratch phase, retained independently of
/// a source journal. The common charge compiler binds its canonical formula to
/// these facts; this consumer never supplies a second charge price.
pub(crate) struct TextEvaluation {
    pub(crate) arguments: [Value; 2],
    pub(crate) result: Value,
    pub(crate) summaries: [lash_kernel_doc::heap::TextSummary; 3],
    pub(crate) work: u64,
    pub(crate) install: u64,
}

/// Detached text storage and actual source-ordered evaluations. No registration,
/// charge acceptance or source frontier escapes the eventual boundary callback.
pub(crate) struct TextPrefix<'a> {
    storage: lash_kernel_state::arena::PreparedTexts<'a>,
    inputs: Vec<lash_kernel_state::kernel_internals::logical::Owner>,
    outputs: Vec<(lash_kernel_state::kernel_internals::logical::Owner, Value)>,
    evaluations: Vec<TextEvaluation>,
}
impl TextPrefix<'_> {
    #[cfg(any(test, feature = "perf-witness"))]
    pub(crate) fn moved(&self) -> u64 {
        self.storage.moved()
    }

    pub(crate) fn release_cost(
        &mut self,
        ledger: &lash_kernel_state::kernel_internals::logical::Ledger,
        heap: &lash_kernel_state::kernel_internals::PagedHeap,
    ) -> Result<u64, HeapFault> {
        self.storage
            .release_cost(ledger, heap, &self.inputs, &self.outputs)
    }
    pub(crate) fn arena(&self) -> &Arena {
        self.storage.arena()
    }
    pub(crate) fn evaluations(&self) -> &[TextEvaluation] {
        &self.evaluations
    }
    pub(crate) fn boundary<R>(
        self,
        ledger: &mut lash_kernel_state::kernel_internals::logical::Ledger,
        heap: &lash_kernel_state::kernel_internals::PagedHeap,
        finish: impl FnOnce(
            lash_kernel_state::kernel_internals::logical::BoundaryTransaction<'_>,
            &mut lash_kernel_state::kernel_internals::logical::Ledger,
            &mut dyn FnMut(
                &mut Arena,
                Vec<lash_kernel_state::kernel_internals::logical::MemberEdit>,
            ),
            &[TextEvaluation],
        ) -> Result<R, HeapFault>,
    ) -> Result<Option<(R, Vec<Value>)>, HeapFault> {
        let live = self.outputs.iter().map(|(_, value)| *value).collect();
        self.storage
            .boundary(
                ledger,
                heap,
                &self.inputs,
                &self.outputs,
                |transaction, ledger, apply| finish(transaction, ledger, apply, &self.evaluations),
            )
            .map(|result| result.map(|result| (result, live)))
    }
}

impl Cx<'_> {
    /// Execute a certified straight-line append kernel, then prepare only its
    /// actual surviving roots. `steps` index entry values followed by results
    /// in source order. The compiler supplies observer exclusion for each move.
    /// The finish callback authenticates the source exit and settles one charge
    /// debt and BoundaryTransaction. No old source/journal wrapper runs here.
    #[expect(
        clippy::too_many_arguments,
        reason = "one source prefix with its storage and exit"
    )]
    pub(crate) fn prepare_text_prefix<'a>(
        arena: &'a mut Arena,
        ledger: &lash_kernel_state::kernel_internals::logical::Ledger,
        _heap: &lash_kernel_state::kernel_internals::PagedHeap,
        entries: &[Value],
        private: &[ArenaHandle],
        join: Option<(usize, usize)>,
        steps: &[(usize, usize, bool)],
        outputs: &[(lash_kernel_state::kernel_internals::logical::Owner, usize)],
        inputs: &[lash_kernel_state::kernel_internals::logical::Owner],
    ) -> Result<Option<TextPrefix<'a>>, HeapFault> {
        let joined = match join {
            Some((list, separator)) => {
                let (Some(&Value::List(list)), Some(&separator)) =
                    (entries.get(list), entries.get(separator))
                else {
                    return Ok(None);
                };
                let Ok(pieces) = arena.sequence(list) else {
                    return Ok(None);
                };
                Some((pieces.to_vec(), separator, Value::List(list)))
            }
            None => None,
        };
        let mut bound_entries = entries.to_vec();
        if let Some((pieces, _, _)) = &joined {
            bound_entries.extend_from_slice(pieces);
        }
        let Some(mut storage) = arena.prepare_text_region(ledger, &bound_entries, private) else {
            return Ok(None);
        };
        let mut values = entries.to_vec();
        let mut evaluations = Vec::with_capacity(steps.len());
        if let Some((pieces, separator, list)) = joined {
            let Some((result, facts)) = storage.join(&pieces, separator) else {
                return Ok(None);
            };
            values.push(result);
            evaluations.push(TextEvaluation {
                arguments: [list, separator],
                result,
                summaries: [facts.left, facts.right, facts.result],
                work: facts.work,
                install: facts.install,
            });
        }
        for &(left, right, moving) in steps {
            let (Some(&left), Some(&right)) = (values.get(left), values.get(right)) else {
                return Ok(None);
            };
            let Some((result, facts)) = storage.concat(left, right, moving) else {
                return Ok(None);
            };
            values.push(result);
            evaluations.push(TextEvaluation {
                arguments: [left, right],
                result,
                summaries: [facts.left, facts.right, facts.result],
                work: facts.work,
                install: facts.install,
            });
        }
        let Some(roots): Option<Vec<_>> = outputs
            .iter()
            .map(|(owner, index)| Some((*owner, *values.get(*index)?)))
            .collect()
        else {
            return Ok(None);
        };
        Ok(Some(TextPrefix {
            storage,
            inputs: inputs.to_vec(),
            outputs: roots,
            evaluations,
        }))
    }
}

impl crate::machine::islands::compiled::Format {
    /// Storage half of the already recognized parts/join/append graph. The
    /// real parts list and all canonical formatting calls precede this kernel;
    /// the common certificate retains their allocation ordinals and peaks.
    /// On a partial source exit, core selects prepare_text_prefix's join-only
    /// prefix and its own recovery map instead of claiming this final write.
    #[expect(
        clippy::too_many_arguments,
        reason = "one format graph and its authenticated exit"
    )]
    #[expect(
        dead_code,
        reason = "staged format kernel awaits core source activation"
    )]
    pub(crate) fn prepare_text_storage<'a>(
        &self,
        arena: &'a mut Arena,
        ledger: &lash_kernel_state::kernel_internals::logical::Ledger,
        heap: &lash_kernel_state::kernel_internals::PagedHeap,
        registers: &[Value],
        private: &[ArenaHandle],
        inputs: &[lash_kernel_state::kernel_internals::logical::Owner],
        output: lash_kernel_state::kernel_internals::logical::Owner,
        move_left: bool,
    ) -> Result<Option<TextPrefix<'a>>, HeapFault> {
        let (Some(&parts), Some(&left)) = (
            registers.get(self.parts.reg as usize),
            registers.get(self.append_args[0] as usize),
        ) else {
            return Ok(None);
        };
        Cx::prepare_text_prefix(
            arena,
            ledger,
            heap,
            &[parts, self.separator.1, left],
            private,
            Some((0, 1)),
            &[(2, 3, move_left)],
            &[(output, 4)],
            inputs,
        )
    }
}
