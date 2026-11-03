//! Canonical detached text storage for one source prefix.
use super::*;

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
