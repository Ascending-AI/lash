/// Post-implementation form of the FIG-1305 fluency smoke. These are the
/// recurring first-shot shapes that the FIG-1304 calculator surface rejected:
/// awaited tools, aggregate promises, ordinary iteration, and common data
/// shaping. Keeping the corpus executable makes later surface regressions loud.
#[test]
fn first_shot_agent_programs_lower_without_missing_standard_library() {
    let programs = [
        r#"
        const pages = await Promise.all([
          web.fetch({ url: "https://example.test/a" }),
          web.fetch({ url: "https://example.test/b" })
        ]);
        const rendered = [];
        for (let i = 0; i < pages.length; i++) { rendered[i] = JSON.stringify(pages[i]); }
        finish(rendered.join("\n"));
        "#,
        r#"
        const rows = Object.entries({ beta: 2, alpha: 1 });
        const labels = [];
        for (let i = 0; i < rows.length; i++) { labels[i] = rows[i].join(":"); }
        finish(labels.join(","));
        "#,
        r#"
        const input = ["one", "two", "three"];
        let total = 0;
        for (let i = 0; i < input.length; i++) { total = total + input[i].length; }
        finish({ total, last: input[input.length - 1].toUpperCase() });
        "#,
        r#"
        const worker = defineProcess({
          name: "worker", signals: { ready: null },
          run: async (input: unknown) => {
            const signal = await waitSignal("ready");
            await sleep(10);
            wake(signal);
            return input;
          }
        });
        finish(await start(worker, { input: Math.max(1, 2) }));
        "#,
    ];

    for source in programs {
        lash_typescript::parse(source).expect("first-shot agent program should lower");
    }
}
