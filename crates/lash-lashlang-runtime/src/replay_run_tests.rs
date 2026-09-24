use super::*;

fn namespace() -> LashlangReplayNamespace {
    LashlangReplayNamespace::cell("session:turn:0:1:exec_code:e3")
}

fn key(ordinal: u64, sub: &str) -> String {
    let command = namespace().command(ordinal);
    if sub.is_empty() {
        command.into_string()
    } else {
        format!("{command}:{sub}")
    }
}

fn run_over(replay_keys: Vec<String>, group_keys: Vec<String>) -> LashlangReplayRun {
    let run = LashlangReplayRun::new(namespace(), LashlangRunOrdinals::start());
    run.state.lock_recover().frontier = Frontier::Recorded(RecordedRun::read(
        &namespace(),
        RecordedKeys {
            settled_keys: replay_keys.clone(),
            replay_keys,
            group_keys,
            closing_outcome: None,
        },
    ));
    run
}

fn issue_to(run: &LashlangReplayRun, ordinal: u64) -> IssuedCommand {
    loop {
        let command = run.issue().expect("issue");
        if command.ordinal == ordinal {
            return command;
        }
    }
}

/// The grammar's spelling is byte-ordered: every ordinal key of a namespace
/// sorts inside its range, before the seal, in ordinal order — so one range
/// read answers every frontier question on SQLite and on a `COLLATE "C"`
/// PostgreSQL column alike.
#[test]
fn ordinal_keys_sort_by_ordinal_and_before_the_seal() {
    let namespace = namespace();
    let range = namespace.range();
    let mut keys = vec![
        key(10, ""),
        key(2, "attempt:1"),
        key(2, "attempt:1:sleep"),
        key(9_999_999_999, "child:3:attempt:2"),
        key(0, "sleep"),
        namespace.seal(),
    ];
    keys.sort();
    assert_eq!(keys.last(), Some(&namespace.seal()));
    for key in &keys {
        assert!(range.lower.as_str() < key.as_str(), "{key}");
        assert!(key.as_str() <= range.upper.as_str(), "{key}");
    }
    assert!(
        key(9, "") < key(10, ""),
        "zero padding keeps byte order ordinal order"
    );
    assert!(
        key(1, "zzz") < key(2, ""),
        "every sub-key of ordinal 1 sorts before ordinal 2"
    );
}

/// No compiler byte reaches a key: two runs of one cell mint the same keys
/// for the same ordinals whatever issued them.
#[test]
fn a_command_key_is_its_namespace_and_ordinal() {
    assert_eq!(
        namespace().command(3).as_str(),
        "session:turn:0:1:exec_code:e3:lk2:0000000003"
    );
    assert_eq!(
        LashlangReplayNamespace::process("5:process:1:w:incarnation:2")
            .command(0)
            .as_str(),
        "lashlang:v2:5:process:1:w:incarnation:2:lk2:0000000000"
    );
}

#[test]
fn a_frontier_read_tells_command_shapes_apart() {
    let run = RecordedRun::read(
        &namespace(),
        RecordedKeys {
            replay_keys: vec![
                key(0, "attempt:1"),
                key(0, "attempt:1:sleep"),
                key(0, "attempt:2"),
                key(1, ""),
                key(2, "sleep"),
                key(3, "child:0"),
                key(3, "child:0:attempt:1"),
                key(3, "timers-admitted"),
                key(4, "process:await:process:subagent:x"),
                key(5, "signal"),
                key(6, ""),
                key(6, "attempt:1"),
                key(7, "nonsense"),
                namespace().seal(),
            ],
            group_keys: vec![key(3, "")],
            settled_keys: vec![key(0, "attempt:1"), key(1, "")],
            closing_outcome: Some("{\"compiler\":\"x\"}".to_string()),
        },
    );
    assert_eq!(
        run.commands.get(&0),
        Some(&RecordedCommand::Shape(CommandShape::ToolCall))
    );
    assert_eq!(
        run.commands.get(&1),
        Some(&RecordedCommand::Shape(CommandShape::Value))
    );
    assert_eq!(
        run.commands.get(&2),
        Some(&RecordedCommand::Shape(CommandShape::Sleep))
    );
    assert_eq!(
        run.commands.get(&3),
        Some(&RecordedCommand::Shape(CommandShape::Aggregate))
    );
    assert_eq!(
        run.commands.get(&4),
        Some(&RecordedCommand::Shape(CommandShape::AwaitHandle))
    );
    assert_eq!(
        run.commands.get(&5),
        Some(&RecordedCommand::Shape(CommandShape::SignalWait))
    );
    assert!(
        matches!(run.commands.get(&6), Some(RecordedCommand::Unreadable(_))),
        "a value and a tool call at one ordinal are no one command"
    );
    assert!(
        matches!(run.commands.get(&7), Some(RecordedCommand::Unreadable(_))),
        "a sub-key no command writes is unreadable"
    );
    assert!(run.sealed);
    assert_eq!(run.seal_outcome.as_deref(), Some("{\"compiler\":\"x\"}"));
}

/// T2/T3 shape: a recorded command of another shape at the ordinal refuses
/// before anything reaches the host.
#[test]
fn a_command_recorded_as_another_shape_refuses_at_its_ordinal() {
    let run = run_over(vec![key(0, "attempt:1")], Vec::new());
    let command = issue_to(&run, 0);
    let divergence = run
        .enter(&command, CommandShape::Value)
        .expect_err("a value where a tool call was recorded refuses");
    let error = divergence.into_error(&SealAttribution::default());
    assert_eq!(error.code, RuntimeErrorCode::LashlangCellReplayDivergence);
    assert!(
        error.message.contains("issue ordinal 0"),
        "{}",
        error.message
    );
    assert!(error.message.contains("tool call"), "{}", error.message);
}

/// T4: a recorded scalar call replayed as an aggregate.
#[test]
fn a_kind_change_at_the_ordinal_refuses() {
    let run = run_over(vec![key(0, "attempt:1")], Vec::new());
    let command = issue_to(&run, 0);
    assert!(run.enter(&command, CommandShape::Aggregate).is_err());

    let run = run_over(vec![key(0, "child:0:attempt:1")], vec![key(0, "")]);
    let command = issue_to(&run, 0);
    assert!(run.enter(&command, CommandShape::ToolCall).is_err());
}

#[test]
fn a_recorded_command_replays_and_the_frontier_goes_live() {
    let run = run_over(vec![key(0, "attempt:1"), key(1, "")], Vec::new());
    let first = issue_to(&run, 0);
    assert!(
        matches!(
            run.enter(&first, CommandShape::ToolCall),
            Ok(CommandAdmission::ReplayRecordedKeys { ref keys, .. })
                if keys.contains(&key(0, "attempt:1"))
        ),
        "a replayed command with entries beyond it is fenced to the recorded keys"
    );
    run.finish(&first, true).expect("a replayed command closes");
    let second = issue_to(&run, 1);
    assert_eq!(
        run.enter(&second, CommandShape::Value),
        Ok(CommandAdmission::Replay)
    );
    run.finish(&second, true).expect("closes");
    let third = issue_to(&run, 2);
    assert_eq!(
        run.enter(&third, CommandShape::Sleep),
        Ok(CommandAdmission::Live),
        "nothing recorded at or beyond the frontier: live"
    );
}

/// The fence: nothing recorded here but entries recorded beyond, so the
/// command may run but its first write is refused.
#[test]
fn an_unrecorded_command_inside_the_recorded_run_refuses_its_writes() {
    let run = run_over(vec![key(0, "attempt:1"), key(2, "")], Vec::new());
    let command = issue_to(&run, 1);
    match run.enter(&command, CommandShape::ToolCall) {
        Ok(CommandAdmission::RefuseWrites(divergence)) => {
            let message = divergence.into_error(&SealAttribution::default()).message;
            assert!(message.contains("ordinal 2"), "{message}");
        }
        other => panic!("expected refused writes, got {other:?}"),
    }
    // A command that indeed writes nothing — as the recorded run did — passes.
    run.finish(&command, false)
        .expect("an unrecorded command that wrote nothing is consistent");
}

#[test]
fn a_sealed_run_refuses_an_extra_trailing_command() {
    let run = run_over(vec![key(0, ""), namespace().seal()], Vec::new());
    let first = issue_to(&run, 0);
    run.enter(&first, CommandShape::Value).expect("replays");
    run.finish(&first, true).expect("closes");
    let extra = issue_to(&run, 1);
    assert!(matches!(
        run.enter(&extra, CommandShape::Value),
        Ok(CommandAdmission::RefuseWrites(_))
    ));
}

/// T5: a dropped trailing command refuses at the seal.
#[test]
fn a_run_that_ends_early_refuses_at_its_seal() {
    let run = run_over(vec![key(0, ""), key(1, "sleep")], Vec::new());
    let first = issue_to(&run, 0);
    run.enter(&first, CommandShape::Value).expect("replays");
    run.finish(&first, true).expect("closes");
    let divergence = run
        .seal()
        .expect_err("ordinal 1 was recorded and never issued");
    assert!(
        divergence
            .into_error(&SealAttribution::default())
            .message
            .contains("at its seal")
    );
}

/// T5/T9: a command the journal recorded that no longer reaches the host
/// refuses at its ordinal.
#[test]
fn a_recorded_command_that_is_no_longer_dispatched_refuses() {
    let run = run_over(vec![key(0, "attempt:1")], Vec::new());
    let command = issue_to(&run, 0);
    run.enter(&command, CommandShape::ToolCall)
        .expect("replays");
    assert!(run.finish(&command, false).is_err());
}

#[test]
fn the_dispatched_digest_names_which_ordinals_wrote() {
    let digest = |wrote: &[bool]| {
        let run = LashlangReplayRun::new(namespace(), LashlangRunOrdinals::start());
        run.state.lock_recover().frontier = Frontier::Positional;
        for wrote in wrote {
            let command = run.issue().expect("issue");
            run.finish(&command, *wrote).expect("closes");
        }
        run.seal().expect("seals").dispatched_ordinals_digest
    };
    assert_eq!(digest(&[true, false]), digest(&[true, false]));
    assert_ne!(digest(&[true, false]), digest(&[false, true]));
    assert_ne!(digest(&[true, true]), digest(&[true, false]));
}

#[test]
fn a_positional_host_admits_every_command_live() {
    let run = LashlangReplayRun::new(namespace(), LashlangRunOrdinals::start());
    run.state.lock_recover().frontier = Frontier::Positional;
    let command = run.issue().expect("issue");
    assert_eq!(
        run.enter(&command, CommandShape::ToolCall),
        Ok(CommandAdmission::Live)
    );
}

#[test]
fn a_resumed_run_continues_its_ordinals() {
    let run = LashlangReplayRun::new(namespace(), LashlangRunOrdinals::start());
    run.issue().expect("issue");
    run.issue().expect("issue");
    let handed_over = run.ordinals();
    let resumed = LashlangReplayRun::new(namespace(), handed_over);
    assert_eq!(resumed.issue().expect("issue").ordinal, 2);
}

/// FIG-3586 (per-key frontier): a replayed handle await whose handle moved —
/// `await h2; await h1` swapped — writes a key the journal does not hold at
/// its ordinal while entries lie beyond it, and its guard refuses the write
/// before anything is awaited live.
#[test]
fn a_replayed_command_cannot_write_a_key_the_journal_does_not_hold() {
    let run = run_over(
        vec![
            key(0, "process:await:process:a"),
            key(1, "process:await:process:b"),
        ],
        Vec::new(),
    );
    let command = issue_to(&run, 0);
    let Ok(CommandAdmission::ReplayRecordedKeys { keys, divergence }) =
        run.enter(&command, CommandShape::AwaitHandle)
    else {
        panic!("a recorded await with entries beyond it is fenced to its recorded keys");
    };
    let range = namespace().range();
    let guard = lash_core::CommandJournalGuard::fenced(lash_core::RecordedKeyFence {
        keys,
        lower: range.lower,
        upper: range.upper,
        refusal: divergence.into_error(&SealAttribution::default()),
    });
    assert!(
        guard
            .admit(Some(&key(0, "process:await:process:a")))
            .is_ok(),
        "the recorded await replays"
    );
    let refusal = guard
        .admit(Some(&key(0, "process:await:process:b")))
        .expect_err("another handle awaited at the ordinal refuses");
    assert_eq!(refusal.code, RuntimeErrorCode::LashlangCellReplayDivergence);
    assert!(guard.tripped().is_some(), "the run stops on the refusal");
}

#[test]
fn a_key_fence_leaves_keys_outside_the_namespace_to_the_host() {
    let run = run_over(vec![key(0, ""), key(1, "")], Vec::new());
    let command = issue_to(&run, 0);
    let Ok(CommandAdmission::ReplayRecordedKeys { keys, divergence }) =
        run.enter(&command, CommandShape::Value)
    else {
        panic!("fenced");
    };
    let range = namespace().range();
    let guard = lash_core::CommandJournalGuard::fenced(lash_core::RecordedKeyFence {
        keys,
        lower: range.lower,
        upper: range.upper,
        refusal: divergence.into_error(&SealAttribution::default()),
    });
    assert!(
        guard
            .admit(Some("effect-group-incorporate:elsewhere"))
            .is_ok()
    );
    assert!(guard.admit(None).is_ok());
    assert!(guard.tripped().is_none());
}
