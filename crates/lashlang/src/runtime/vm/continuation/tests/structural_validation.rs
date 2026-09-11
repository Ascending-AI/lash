use super::*;
use crate::ExecutionHostError;

/// Serde supplies its complete accepted variant vocabulary to deserialize_enum.
/// Keep this independent pin: a new error can reach a suspended finally's origin
/// even when none of the continuation structs change.
#[test]
fn continuation_runtime_error_wire_variants_are_pinned() {
    struct VariantProbe;
    impl<'de> serde::Deserializer<'de> for VariantProbe {
        type Error = serde::de::value::Error;

        fn deserialize_any<V: serde::de::Visitor<'de>>(
            self,
            _visitor: V,
        ) -> Result<V::Value, Self::Error> {
            Err(serde::de::Error::custom("expected RuntimeError enum"))
        }

        fn deserialize_enum<V: serde::de::Visitor<'de>>(
            self,
            name: &'static str,
            variants: &'static [&'static str],
            _visitor: V,
        ) -> Result<V::Value, Self::Error> {
            assert_eq!(name, "RuntimeError");
            assert_eq!(
                variants,
                &[
                    "FrameDepthExceeded",
                    "FunctionIndexOverflow",
                    "NonFunctionCall",
                    "FunctionArgumentCount",
                    "UnknownFunction",
                    "ClosureCaptureCountMismatch",
                    "FunctionValueAtHostBoundary",
                    "JavaScriptExoticAtHostBoundary",
                    "EffectInBuiltinCallback",
                    "InstructionBudgetExceeded",
                    "RegExpBudgetExceeded",
                    "ExecutionDeadlineExceeded",
                    "MemoryLimitExceeded",
                    "HostCancelled",
                    "DanglingHeapReference",
                    "HeapIdExhausted",
                    "UnexportedHeapReference",
                    "CyclicHostValue",
                    "ValueDepthLimitExceeded",
                    "UndefinedVariable",
                    "NonListIteration",
                    "SessionProcessAdminOutsideProcess",
                    "ForegroundControlInsideProcess",
                    "UnknownBuiltin",
                    "CannotReadField",
                    "ToolResultExpected",
                    "ToolResultMissingValue",
                    "ToolResultInvalidOk",
                    "CannotIndex",
                    "ImmutableImageFields",
                    "ImmutableImageFieldsThrough",
                    "ImmutableTupleIndexes",
                    "ImmutableTupleIndexesThrough",
                    "CannotAssignField",
                    "CannotAssignThroughField",
                    "CannotAssignIndex",
                    "CannotAssignThroughIndex",
                    "InvalidListAssignmentIndex",
                    "TypeScriptArrayNonIndexPropertyUnsupported",
                    "PendingTool",
                    "InvalidArgumentCount",
                    "EmptyUnsupported",
                    "KeysUnsupported",
                    "ValuesUnsupported",
                    "SliceUnsupported",
                    "FormatTemplateMissing",
                    "FormatTemplateInvalid",
                    "LenUnsupported",
                    "ContainsUnsupported",
                    "InUnsupported",
                    "JoinUnsupported",
                    "PushUnsupported",
                    "ShapingListRequired",
                    "ShapingTextRequired",
                    "ShapingNumberRequired",
                    "ShapingComparableRequired",
                    "ShapingEmptyList",
                    "SortByRecordRequired",
                    "SortByEmptyPath",
                    "SortByMissingPath",
                    "InvalidRangeBound",
                    "InvalidRangeBoundType",
                    "InvalidIntegerDivisionArgument",
                    "InvalidIntegerDivisionArgumentType",
                    "ExpectedNumber",
                    "ExpectedNumberType",
                    "ExpectedText",
                    "InvalidIndex",
                    "InvalidCharacterIndex",
                    "IncompatibleSequenceConcatenation",
                    "ReadOnlyProjectedBinding",
                    "ValidateTypeLiteralRequired",
                    "NotTypeValue",
                    "UnwrappedToolResultFailed",
                    "UnwrappedHostToolResultFailed",
                    "UnwrappedModuleOperationFailed",
                    "MissingAssignmentIndex",
                    "MissingAssignmentField",
                    "MissingAssignmentKey",
                    "ListAssignmentIndexOutOfBounds",
                    "InvalidJson",
                    "EmptyGrepNeedle",
                    "Format",
                    "ZeroRangeStep",
                    "RangeTooLarge",
                    "IntegerDivisionByZero",
                    "UnknownProcess",
                    "ProcessNotExported",
                    "ProcessRefNotExported",
                    "ArtifactProcessMissing",
                    "ValidationFailed",
                    "StartSiteMissing",
                    "LinkedArtifactMissing",
                    "LinkedProcessNotExported",
                    "ProcessStartFailed",
                    "SleepFailed",
                    "WaitSignalFailed",
                    "SignalRunFailed",
                    "CancelFailed",
                    "ProcessEventFailed",
                    "PrintFailed",
                    "FinishFailed",
                    "FailFailed",
                    "ResourceBatchReceiverOutOfRange",
                    "ResourceBatchArgumentOutOfRange",
                    "InvalidResourceBatchResult",
                    "ResourceBatchFailed",
                    "ResourceBatchResultCount",
                    "ResourceBatchSettlementOrder",
                    "AwaitExpectsHandle",
                    "ResourceListBatchMalformed",
                    "AggregateAwaitLeafOutOfRange",
                    "AggregateAwaitValueOutOfRange",
                    "InvalidAggregateAwaitRecordShape",
                    "VmStackUnderflow",
                    "MissingLoopState",
                    "ContextDependentIntrinsicMisdispatch",
                    "UncaughtException",
                    "InvalidExceptionState",
                ],
                "RuntimeError changed the continuation wire; review its format version"
            );
            Err(serde::de::Error::custom("variant vocabulary checked"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes
            byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map struct identifier ignored_any
        }
    }
    let error = RuntimeError::deserialize(VariantProbe).unwrap_err();
    assert_eq!(error.to_string(), "variant vocabulary checked");
}

#[test]
fn awaited_settled_value_survives_a_finally_origin_wire_roundtrip() {
    let origin = VmPendingErrorOriginContinuation {
        error: RuntimeError::AwaitExpectsHandle {
            found: "number at `orders[0]`".into(),
        },
        instruction_pointer: 0,
        span: None,
    };
    let mut continuation = empty_continuation(Heap::default());
    continuation.finally_stack.push(VmFinallyContinuation {
        completion: VmFinallyCompletionContinuation::Throw {
            value: Value::Null,
            origin: Some(origin),
        },
        handler_stack_depth: 0,
        frame_depth: 0,
        frame_function: None,
        operand_stack_depth: 0,
    });
    validate_continuation(&continuation).unwrap();
    let wire = serde_json::to_value(&continuation).unwrap();
    assert_eq!(
        wire["finally_stack"][0]["completion"]["origin"]["error"],
        serde_json::json!({
            "AwaitExpectsHandle": { "found": "number at `orders[0]`" }
        })
    );
    let decoded = serde_json::from_value::<VmContinuation>(wire).unwrap();
    validate_continuation(&decoded).unwrap();
    assert_eq!(decoded.finally_stack, continuation.finally_stack);
}

#[test]
fn structured_tool_failure_survives_a_finally_origin_wire_roundtrip() {
    let source = ExecutionHostError::from_tool_failure(&lash_sansio::ToolFailure {
        class: lash_sansio::ToolFailureClass::PermissionDenied,
        code: "approval_denied".to_string(),
        message: "approval was denied".to_string(),
        source: lash_sansio::ToolFailureSource::Policy,
        retry: lash_sansio::ToolRetryStatus::Exhausted { attempts: 3 },
        raw: None,
    });
    let origin = VmPendingErrorOriginContinuation {
        error: RuntimeError::UnwrappedHostToolResultFailed { source },
        instruction_pointer: 0,
        span: None,
    };
    let mut continuation = empty_continuation(Heap::default());
    continuation.finally_stack.push(VmFinallyContinuation {
        completion: VmFinallyCompletionContinuation::Throw {
            value: Value::Null,
            origin: Some(origin),
        },
        handler_stack_depth: 0,
        frame_depth: 0,
        frame_function: None,
        operand_stack_depth: 0,
    });
    validate_continuation(&continuation).unwrap();

    let wire = serde_json::to_value(&continuation).unwrap();
    assert_eq!(wire["format_version"], serde_json::json!(10));
    assert_eq!(
        wire["finally_stack"][0]["completion"]["origin"]["error"],
        serde_json::json!({
            "UnwrappedHostToolResultFailed": {
                "source": {
                    "message": "approval was denied",
                    "tool_failure": {
                        "class": "permission_denied",
                        "code": "approval_denied",
                        "source": "policy",
                        "retry": { "type": "exhausted", "attempts": 3 }
                    }
                }
            }
        })
    );
    let decoded = serde_json::from_value::<VmContinuation>(wire).unwrap();
    validate_continuation(&decoded).unwrap();
    assert_eq!(decoded.finally_stack, continuation.finally_stack);
}
