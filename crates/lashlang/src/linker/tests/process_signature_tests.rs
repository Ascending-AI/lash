use super::*;

#[test]
fn trigger_target_uses_the_scoped_callable_when_it_shadows_a_declaration() {
    let wrong_name = crate::parse(
        r#"
        process scan(event: timer.Tick) -> bool { finish true }
        process install(scan: Process<(payload: timer.Tick), bool>) -> bool {
          source = timer.Schedule({ expr: "0 8 * * *" })
          await triggers.register({
            source: source,
            subscription_key: "shadowed-wrong-name",
            target: scan,
            inputs: { event: trigger.event }
          })?
          finish true
        }
        "#,
    )
    .expect("shadowed process source parses");
    assert!(matches!(
        LinkedModule::link(wrong_name, full_host_environment()),
        Err(LinkError::UnknownTriggerInput { ref input, .. }) if input == "event"
    ));

    let correct_name = crate::parse(
        r#"
        process scan(event: timer.Tick) -> bool { finish true }
        process install(scan: Process<(payload: timer.Tick), bool>) -> bool {
          source = timer.Schedule({ expr: "0 8 * * *" })
          await triggers.register({
            source: source,
            subscription_key: "shadowed-correct-name",
            target: scan,
            inputs: { payload: trigger.event }
          })?
          finish true
        }
        "#,
    )
    .expect("shadowed process source parses");
    LinkedModule::link(correct_name, full_host_environment())
        .expect("scoped process parameter signature is authoritative");
}

#[test]
fn trigger_list_accepts_same_signature_alias_branch_targets() {
    let program = crate::parse(
        r#"
        type Handler = Process<(event: timer.Tick), bool>
        process scan(event: timer.Tick) -> bool { finish true }
        process install(handler: Handler) -> bool {
          selected = handler
          if true { selected = scan } else { selected = handler }
          await triggers.list({ target: selected })?
          finish true
        }
        "#,
    )
    .expect("indirect list target parses");
    LinkedModule::link(program, full_host_environment())
        .expect("list uses the same normalized process target contract as registration");
}
