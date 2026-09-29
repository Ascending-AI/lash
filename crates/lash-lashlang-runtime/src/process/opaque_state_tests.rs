//! The parent holds a segment's VM state as opaque bytes (ADR 0123): its
//! decode of the envelope never reaches the VM's semantic decoder, which
//! validates and compiles regular expressions from guest-controlled bytes.

use super::{
    LASHLANG_SEGMENT_STATE_VERSION, LashlangSegmentState, ReplayOrdinalsState,
    decode_lashlang_segment_state, segment_continuation_expectation, segment_continuation_owner,
    worker_side,
};

/// Answers whether `T` implements `DeserializeOwned`, at compile time: the
/// inherent constant exists only where the bound holds, and the trait
/// constant is what every other type resolves to.
struct DeserializeProbe<T>(std::marker::PhantomData<T>);

trait NoDeserialize {
    const DESERIALIZES: bool = false;
}

impl<T> NoDeserialize for DeserializeProbe<T> {}

impl<T: serde::de::DeserializeOwned> DeserializeProbe<T> {
    const DESERIALIZES: bool = true;
}

// The type-level half: no serde envelope can decode a continuation, and the
// envelope the parent decodes holds its VM state as the protocol's opaque
// bytes, from a crate with no path to the VM.
const _: () = assert!(!DeserializeProbe::<lashlang::VmContinuation>::DESERIALIZES);
const _: () = assert!(DeserializeProbe::<lash_vm_protocol::OpaqueVmState>::DESERIALIZES);
const _: () = assert!(DeserializeProbe::<LashlangSegmentState>::DESERIALIZES);
const _: fn(&LashlangSegmentState) -> &lash_vm_protocol::OpaqueVmState = |state| &state.vm;

struct SleepHost;

impl lashlang::ExecutionHost for SleepHost {
    async fn perform(
        &self,
        op: lashlang::AbilityOp,
    ) -> Result<lashlang::AbilityResult, lashlang::ExecutionHostError> {
        match op {
            lashlang::AbilityOp::Sleep(_) => {
                Ok(lashlang::AbilityResult::Value(lashlang::Value::Null))
            }
            _ => Err(lashlang::ExecutionHostError::new("the witness only sleeps")),
        }
    }
}

/// A process body parked after its first effect, holding a RegExp.
async fn parked_regexp_continuation() -> Vec<u8> {
    use lashlang::testing::ast_builders as b;

    let program = b::program(vec![
        b::assign(
            "pattern",
            b::builtin(
                "__typescript_heap_new",
                vec![b::string("RegExp"), b::string("ab+c"), b::string("")],
            ),
        ),
        b::sleep_for(b::num(1.0)),
        b::finish(b::null()),
    ]);
    let compiled =
        lashlang::testing::harness::try_compile_program(&program).expect("compile the witness");
    let mut state = lashlang::State::new();
    let environment = lashlang::ExecutionEnvironment::new(&SleepHost).process();
    let mut vm =
        lashlang::Vm::from_state(&compiled, &mut state, &environment).expect("install the witness");
    assert_eq!(
        vm.run_process_until_effect()
            .await
            .expect("park after the sleep"),
        lashlang::VmRunOutcome::EffectCompleted
    );
    vm.suspend()
        .expect("park the witness")
        .to_bytes()
        .expect("encode the witness")
}

/// The behavioural half: a segment whose continuation holds a RegExp the VM's
/// validator refuses decodes and passes every parent-side check — the parent
/// never ran the validator — and only the worker's semantic decode refuses it.
#[tokio::test(flavor = "current_thread")]
async fn parent_state_decode_never_compiles_regexp() {
    let bytes = parked_regexp_continuation().await;
    let text = String::from_utf8(bytes).expect("the continuation wire is JSON text");
    assert!(text.contains("\"ab+c\""), "the witness parks its RegExp");
    // An unbalanced group: `validate_typescript_regexp` refuses it.
    let poisoned = text.replacen("\"ab+c\"", "\"ab+(c\"", 1).into_bytes();

    let process_id = lash_sansio::ProcessId::fixture("regexp-witness");
    let owner = segment_continuation_owner(&process_id);
    let vm_contract = lashlang::vm_contract_identity();
    let envelope = serde_json::to_vec(&LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm: lash_vm_protocol::OpaqueVmState::seal(
            lash_vm_protocol::VmStateKind::Continuation,
            owner.clone(),
            vm_contract.clone(),
            lashlang::VM_CONTINUATION_FORMAT_VERSION,
            poisoned,
        ),
        ordinals: ReplayOrdinalsState {
            commands: crate::LashlangRunOrdinals::start(),
            event_sequence: 0,
            signal_wait_ordinals: Default::default(),
        },
        started_process_ids: Vec::new(),
        incorporation_ledger: lash_core::session::IncorporationLedger::default(),
        pending_summary: Vec::new(),
        effect_omissions: std::collections::BTreeMap::new(),
        outstanding_groups: Vec::new(),
    })
    .expect("encode the envelope");

    let decoded = decode_lashlang_segment_state(&envelope)
        .expect("the parent decodes the envelope without touching the VM bytes");
    assert_eq!(
        decoded
            .vm
            .check(&segment_continuation_expectation(&owner, &vm_contract)),
        Ok(()),
        "the parent's structural check passes bytes the VM would refuse"
    );

    let refusal = worker_side::open_continuation(&decoded.vm)
        .expect_err("the worker's semantic decode validates the RegExp");
    assert!(
        refusal.to_string().contains("RegExp"),
        "the refusal is the RegExp validation: {refusal}"
    );
}
