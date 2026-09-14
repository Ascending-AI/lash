/// FIG-2997: a trigger target written inline is a process literal in argument
/// position. It projects as its own process container of the module — named
/// exactly what the linker lifts the literal to — and render -> parse ->
/// render is a fixed point over the call that carries it, the same laws the
/// process corpora prove.
#[test]
fn inline_trigger_target_projects_as_a_process_container_and_fixpoints() {
    let literal_source = r#"await triggers.register({
  source: { expr: "0 8 * * *" },
  target: async (event) => {
    await display.set_status({ key: "review", value: "waking" });
    await display.show_message({ text: "New item to review" });
    return "reviewed";
  },
});
"#;
    let graph = lash_typescript::workflow_graph::workflow_graph_from_source(literal_source)
        .expect("inline target projects");
    let lifted = graph
        .declarations
        .iter()
        .find_map(|declaration| match declaration {
            lashlang::WorkflowDeclaration::Process(process)
                if process.name.starts_with("__process_") =>
            {
                Some(process.name.clone())
            }
            _ => None,
        })
        .expect("the inline body projects as a process container");
    let rendered =
        lash_typescript::workflow_graph::workflow_graph_to_source(&graph).expect("renders");
    assert_eq!(
        lash_typescript::workflow_graph::workflow_graph_from_source(&rendered).expect("reprojects"),
        graph,
        "the inline body's PutGet holds"
    );
    let _ = lifted;
}
