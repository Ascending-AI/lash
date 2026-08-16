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
use crate::runtime::{Chunk, IntrinsicOp, RuntimeError};

/// How much of the operand stack an instruction needs exported to tree values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StackExport {
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
) -> Result<InstructionHeapPlan, RuntimeError> {
    use Instruction as I;
    use StackExport::Top;

    // Keep this match exhaustive. Compile-time failure is the guard that makes
    // every new opcode author declare its heap boundary explicitly; a catch-all
    // would silently restore the old conservative default and hide omissions.
    let plan = match instruction {
        // Isolation and in-place container mutation consume heap references as
        // they are: exporting them would be the copy these opcodes exist to
        // avoid.
        I::DeepCopy
        | I::DeepCopyLoopBinding(_)
        | I::AppendAssign(_)
        | I::ListAppend
        | I::Intrinsic(IntrinsicOp::PushAssign(_))
        | I::MakeClosure { .. }
        | I::Call { .. }
        | I::CallDynamic
        | I::Map
        | I::AsyncMap
        | I::Return
        | I::Throw
        | I::BuildTuple(_)
        | I::BuildList(_)
        | I::BuildHeapList(_)
        | I::BuildRecord(_)
        | I::BuildHeapRecord(_)
        | I::Duplicate
        | I::JavaScriptUnary(_)
        | I::JavaScriptBinary(_)
        | I::Intrinsic(
            IntrinsicOp::JavaScriptSplit
            | IntrinsicOp::JavaScriptJoin
            | IntrinsicOp::JavaScriptStdlib(_)
            | IntrinsicOp::JavaScriptHeapNew(_)
            | IntrinsicOp::JavaScriptHeapInstanceOf
            | IntrinsicOp::JavaScriptGlobalDelete
            | IntrinsicOp::JavaScriptGlobalHas
            | IntrinsicOp::JavaScriptGlobalSet
            | IntrinsicOp::JavaScriptUriCodec(_),
        )
        | I::IsNullish => InstructionHeapPlan::heap_native(),
        I::StoreName(_) => InstructionHeapPlan::heap_native(),
        I::HeapPathAssign { .. } => InstructionHeapPlan::heap_native(),
        // These export the operands they need through the heap themselves.
        I::AddAssignIndexNumber { .. } | I::AddAssignIndexSlotNumber { .. } => {
            InstructionHeapPlan::heap_native()
        }
        // Structural equality compares heap objects by walking them.
        I::Binary(BinaryOp::Equal | BinaryOp::NotEqual) => InstructionHeapPlan::heap_native(),

        // Pure pushes and jumps read nothing from the stack.
        I::PushConst(_)
        | I::PushNull
        | I::PushUndefined
        | I::PushBool(_)
        | I::PushNumber(_)
        | I::LoadName(_)
        | I::StoreConst { .. }
        | I::Jump(_)
        | I::IterNext { .. }
        | I::EndIter
        | I::ObserveStep
        | I::PushHandler { .. }
        | I::PopHandler
        | I::EnterFinally { .. }
        | I::EndFinally
        | I::AbandonFinally => InstructionHeapPlan::stack(Top(0)),
        I::AbandonFinallyKeepValue => InstructionHeapPlan::heap_native(),

        // Single-operand opcodes.
        I::Field(_)
        | I::ResultUnwrap
        | I::ToBool
        | I::JumpIfFalse(_)
        | I::JumpIfTrue(_)
        | I::Unary(_)
        | I::Pop
        | I::BeginIter(_) => InstructionHeapPlan::stack(Top(1)),

        // Two-operand opcodes.
        I::Index | I::Binary(_) | I::JumpIfCompareFalse { .. } => {
            InstructionHeapPlan::stack(Top(2))
        }

        // Opcodes whose operand count is carried in the instruction, or in the
        // table the instruction points at.
        I::BeginRangeIter { argc, .. } => InstructionHeapPlan::stack(Top(argc)),
        I::ResourceCall { argc, .. } | I::ResourceCallUnwrap { argc, .. } => {
            InstructionHeapPlan::stack(Top(argc + 1))
        }
        I::ResourceOperationBatch(batch) => InstructionHeapPlan::stack(Top(chunk
            .resource_operation_batches[batch]
            .stack_value_count)),
        I::StartProcess { keys, .. } => {
            InstructionHeapPlan::stack(Top(chunk.key_lists[keys].len()))
        }

        I::Print
        | I::Finish
        | I::SleepFor
        | I::SleepUntil
        | I::AwaitHandle
        | I::AwaitHandleUnwrap
        | I::CancelHandle
        | I::WrapTypeLiteral
        | I::WrapHostDescriptor(_)
        | I::ProcessYield
        | I::ProcessWake
        | I::ProcessFail => InstructionHeapPlan::stack(Top(1)),
        I::ProcessWaitSignal { .. } => InstructionHeapPlan::stack(Top(0)),
        I::ProcessSignalRun { .. } => InstructionHeapPlan::stack(Top(2)),

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

        I::Intrinsic(IntrinsicOp::FormatCompiled(template)) => {
            InstructionHeapPlan::stack(Top(chunk.format_templates[template].argc))
        }

        // Every remaining intrinsic reads exactly its argument count from the
        // stack — that same count is what dispatch uses to find them — and
        // touches no slot. The three that do carry a slot are declared above.
        I::Intrinsic(op) => InstructionHeapPlan::stack(Top(op.fixed_argc().ok_or(
            RuntimeError::ContextDependentIntrinsicMisdispatch {
                context: "heap planning".into(),
            },
        )?)),
    };
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_opcodes_keep_closure_references_inside_the_vm() {
        use crate::runtime::Instruction as I;
        let program = crate::compile("finish 0").expect("a trivial program compiles");
        let chunk = &program.chunk;
        let function_opcodes = [
            I::MakeClosure {
                function: 0,
                captures: 1,
            },
            I::Call { argc: 1 },
            I::CallDynamic,
            I::Map,
            I::AsyncMap,
            I::Return,
        ];
        for (index, instruction) in function_opcodes.into_iter().enumerate() {
            let plan = instruction_heap_plan(instruction, chunk).expect("function heap plan");
            assert_eq!(
                (plan.stack, plan.slots),
                (StackExport::Top(0), SlotExport::None),
                "function opcode {index} must not export a closure"
            );
        }
    }
}
