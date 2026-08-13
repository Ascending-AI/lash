// The one place an opcode declares what it needs from heap-backed state.
//
// Three separate hand-maintained lists used to answer this question — one for
// "operates on heap values directly", one for how much of the operand stack to
// export, one for which slots to export — and an opcode could be in the right
// place in two of them and missing from the third. That is exactly how
// `format("{0}", xs)` came to hand a heap reference to the stringifier: the
// fused slot-format opcodes read a slot, and only the slot list knew about
// slots. One plan per opcode means a new opcode is either declared here or gets
// the conservative default, and there is no third list to forget.

use super::Instruction;
use crate::ast::BinaryOp;
use crate::runtime::{Chunk, IntrinsicOp};

/// How much of the operand stack an instruction needs exported to tree values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StackExport {
    /// Export every operand. The conservative default for opcodes that are not
    /// declared below, and for opcodes that read the stack by a length the plan
    /// cannot see.
    All,
    /// Export the top `n` operands and leave anything deeper alone.
    ///
    /// Leaving a reference deeper on the stack is what keeps an accumulator
    /// under a loop body from being rebuilt on every instruction. A reference
    /// left there stays safe: it is a collection root, it serializes into a
    /// continuation, and the terminal export walks the whole stack.
    Top(usize),
}

/// Which slots an instruction needs exported, and whether it reads them or
/// mutates through them — which decides whether the materialization may keep
/// the boundary cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SlotExport {
    /// No slot is read: the opcode works on the stack, or on heap references
    /// directly.
    None,
    Read(usize),
    Mutate(usize),
    /// Export every slot read-only. The conservative default, for the same
    /// reason `StackExport::All` is: an opcode nobody declared may read any
    /// slot, and a heap reference reaching a boundary that expects a tree is
    /// how `format("{0}", xs)` came to take the process down. Being
    /// conservative on one axis and permissive on the other would have let the
    /// next undeclared slot reader reproduce it quietly.
    All,
}

/// Everything one instruction needs from heap-backed state before it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct InstructionHeapPlan {
    pub(super) stack: StackExport,
    pub(super) slots: SlotExport,
}

impl InstructionHeapPlan {
    const fn stack(stack: StackExport) -> Self {
        Self {
            stack,
            slots: SlotExport::None,
        }
    }

    /// An opcode that works on heap references directly: nothing is exported for
    /// it, and it reaches into the heap itself.
    const fn heap_native() -> Self {
        Self::stack(StackExport::Top(0))
    }

    const fn with_read_slot(mut self, slot: usize) -> Self {
        self.slots = SlotExport::Read(slot);
        self
    }

    const fn with_mutable_slot(mut self, slot: usize) -> Self {
        self.slots = SlotExport::Mutate(slot);
        self
    }
}

/// The heap contract for one opcode.
///
/// Declaring an opcode here is a claim about its implementation: that it reads
/// at most the stated operands and slots. Getting it wrong leaves a reference
/// somewhere that expects a tree, which now fails the cell with a typed error
/// rather than taking down the process — but it is still wrong, so the claim is
/// what the audit checks.
pub(super) fn instruction_heap_plan(
    instruction: Instruction,
    chunk: &Chunk,
) -> InstructionHeapPlan {
    use Instruction as I;
    use StackExport::{All, Top};

    match instruction {
        // Isolation and in-place container mutation consume heap references as
        // they are: exporting them would be the copy these opcodes exist to
        // avoid.
        I::DeepCopy
        | I::DeepCopyLoopBinding(_)
        | I::AppendAssign(_)
        | I::ListAppend
        | I::Intrinsic(IntrinsicOp::PushAssign(_)) => InstructionHeapPlan::heap_native(),
        // These export the operands they need through the heap themselves.
        I::AddAssignIndexNumber { .. } | I::AddAssignIndexSlotNumber { .. } => {
            InstructionHeapPlan::heap_native()
        }
        // Structural equality compares heap objects by walking them.
        I::Binary(BinaryOp::Equal | BinaryOp::NotEqual) => InstructionHeapPlan::heap_native(),

        // Pure pushes and jumps read nothing from the stack.
        I::PushConst(_)
        | I::PushNull
        | I::PushBool(_)
        | I::PushNumber(_)
        | I::LoadName(_)
        | I::StoreConst { .. }
        | I::Jump(_)
        | I::IterNext { .. }
        | I::EndIter
        | I::ObserveStep => InstructionHeapPlan::stack(Top(0)),

        // Single-operand opcodes.
        I::Field(_)
        | I::ResultUnwrap
        | I::ToBool
        | I::JumpIfFalse(_)
        | I::JumpIfTrue(_)
        | I::Unary(_)
        | I::Pop
        | I::StoreName(_)
        | I::BeginIter(_) => InstructionHeapPlan::stack(Top(1)),

        // Two-operand opcodes.
        I::Index | I::Binary(_) | I::JumpIfCompareFalse { .. } => {
            InstructionHeapPlan::stack(Top(2))
        }

        // Opcodes whose operand count is carried in the instruction, or in the
        // table the instruction points at.
        I::BuildTuple(len) | I::BuildList(len) => InstructionHeapPlan::stack(Top(len)),
        I::BuildRecord(keys) => InstructionHeapPlan::stack(Top(chunk.key_lists[keys].len())),
        I::BeginRangeIter { argc, .. } => InstructionHeapPlan::stack(Top(argc)),

        // Slot readers. The fused format opcodes belong here: they read a slot
        // and stringify it, so a heap reference in that slot has to be exported
        // exactly like the arithmetic readers below.
        I::LoadField { slot, .. }
        | I::LoadFieldUnwrap { slot, .. }
        | I::ResolveTypeRef(slot)
        | I::Intrinsic(IntrinsicOp::FormatCompiledSlotNumber { slot, .. })
        | I::Intrinsic(IntrinsicOp::FormatCompiledSlotNumberBinary { slot, .. }) => {
            InstructionHeapPlan::stack(Top(0)).with_read_slot(slot)
        }
        I::SlotNumberBinary { slot, .. }
        | I::SlotNumberCompare { slot, .. }
        | I::SlotNumberBinaryCompare { slot, .. }
        | I::JumpIfSlotNumberCompareFalse { slot, .. }
        | I::JumpIfSlotNumberBinaryCompareFalse { slot, .. } => {
            InstructionHeapPlan::stack(Top(0)).with_read_slot(slot)
        }

        // Slot mutators. These rebuild the slot's value in place, so their slot
        // is exported for mutation, which drops the boundary cache first.
        // A path assignment reads the value it stores plus one operand per
        // dynamic index step below it.
        I::PathAssign { slot, path } => {
            InstructionHeapPlan::stack(Top(1 + chunk.assign_paths[path].dynamic_index_count))
                .with_mutable_slot(slot)
        }
        // A compound assignment extends its accumulator in place when both
        // sides are lists, so neither the operand nor the slot is exported up
        // front; the fallback path materializes what it needs.
        I::AddAssign(_) => InstructionHeapPlan::heap_native(),
        I::AddAssignNumber { slot, .. } => {
            InstructionHeapPlan::stack(Top(0)).with_mutable_slot(slot)
        }
        I::AddAssignSlot { .. } => InstructionHeapPlan::heap_native(),

        // Every remaining intrinsic reads exactly its argument count from the
        // stack — that same count is what dispatch uses to find them — and
        // touches no slot. The three that do carry a slot are declared above.
        I::Intrinsic(op) => InstructionHeapPlan::stack(Top(op.argc())),

        // Everything else exports the whole stack and every slot: effects hand
        // values to the host, and an opcode added later lands here until
        // someone declares it.
        _ => InstructionHeapPlan {
            stack: All,
            slots: SlotExport::All,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Which opcodes fall to the conservative default.
    ///
    /// The default exports the whole stack and every slot, which is correct but
    /// costs more than a declaration. This test names the set so growing it is a
    /// deliberate act: an opcode that lands here because nobody declared it will
    /// show up as a change to this list, and an opcode that is expensive to
    /// leave here can be seen before it is measured.
    #[test]
    fn only_effect_shaped_opcodes_use_the_conservative_default() {
        use crate::runtime::Instruction as I;
        let program = crate::compile("finish 0").expect("a trivial program compiles");
        let chunk = &program.chunk;
        let undeclared = [
            I::Print,
            I::Finish,
            I::SleepFor,
            I::SleepUntil,
            I::AwaitHandle,
            I::AwaitHandleUnwrap,
            I::CancelHandle,
            I::WrapTypeLiteral,
            I::ProcessYield,
            I::ProcessWake,
            I::ProcessFail,
        ];
        for (index, instruction) in undeclared.into_iter().enumerate() {
            let plan = instruction_heap_plan(instruction, chunk);
            assert_eq!(
                (plan.stack, plan.slots),
                (StackExport::All, SlotExport::All),
                "undeclared opcode {index} should use the conservative default"
            );
        }
    }
}
