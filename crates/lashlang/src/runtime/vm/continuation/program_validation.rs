use super::*;

fn function_code_range(
    chunk: &Chunk,
    function: Option<u32>,
) -> Result<(std::ops::Range<usize>, String), ContinuationError> {
    match function {
        None => Ok((0..chunk.root_code_len, "root".to_string())),
        Some(index) => {
            let index_usize = index as usize;
            let compiled = chunk
                .functions
                .get(index_usize)
                .ok_or(ContinuationError::UnknownFunction { index })?;
            Ok((
                compiled.entry_ip..compiled.end_ip,
                format!("function {index}"),
            ))
        }
    }
}

pub(super) fn validate_program_continuation(
    continuation: &VmContinuation,
    chunk: &Chunk,
) -> Result<(), ContinuationError> {
    let (active_range, active_owner) = function_code_range(chunk, continuation.active_function)?;
    if !active_range.contains(&continuation.instruction_pointer)
        && continuation.instruction_pointer != active_range.end
    {
        return Err(ContinuationError::InstructionPointerOutsideCodeRange {
            location: "active".to_string(),
            instruction_pointer: continuation.instruction_pointer,
            owner: active_owner,
            range_start: active_range.start,
            range_end: active_range.end,
        });
    }

    for (frame_index, frame) in continuation.frame_stack.iter().enumerate() {
        let (caller_range, caller_owner) = function_code_range(chunk, frame.function)?;
        if !caller_range.contains(&frame.return_instruction_pointer) {
            return Err(ContinuationError::InstructionPointerOutsideCodeRange {
                location: format!("frame {frame_index} return"),
                instruction_pointer: frame.return_instruction_pointer,
                owner: caller_owner,
                range_start: caller_range.start,
                range_end: caller_range.end,
            });
        }
        let call_instruction = frame
            .return_instruction_pointer
            .checked_sub(1)
            .and_then(|index| chunk.code.get(index));
        if !matches!(
            call_instruction,
            Some(Instruction::Call { .. } | Instruction::Map)
        ) {
            return Err(ContinuationError::InvalidReturnSite {
                frame: frame_index,
                instruction_pointer: frame.return_instruction_pointer,
            });
        }
    }

    continuation
        .heap
        .heap
        .validate_closures(&chunk.functions)
        .map_err(|error| match error {
            RuntimeError::UnknownFunction { index } => ContinuationError::UnknownFunction { index },
            RuntimeError::ClosureCaptureCountMismatch {
                index,
                expected,
                actual,
            } => ContinuationError::ClosureCaptureCountMismatch {
                index,
                expected,
                actual,
            },
            other => unreachable!("closure validation returned unrelated error: {other}"),
        })
}
