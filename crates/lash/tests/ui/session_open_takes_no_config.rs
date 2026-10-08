// FIG-4112: an open takes no session config. Model, parent and plugin
// options (the protocol's prompt among them) are creation config, stated once
// in the `SessionCreation` passed to `create`; the open builder cannot carry
// them.

fn open_takes_no_llm_profile(core: lash::LashCore, model: lash::LlmProfileKey) {
    let _ = core
        .session(lash::SessionId::parse("stated-at-open").expect("nonblank host identity"))
        .session_spec(lash::SessionSpec::new(
            model,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        ).no_progress_budget(lash::NoProgressBudget::bounded(12)))
        .open();
}

fn open_takes_no_parent(core: lash::LashCore) {
    let _ = core.session(lash::SessionId::parse("stated-at-open").expect("nonblank host identity")).parent("parent").open();
}

fn open_takes_no_plugin_options(core: lash::LashCore, options: lash::plugins::PluginOptions) {
    let _ = core.session(lash::SessionId::parse("stated-at-open").expect("nonblank host identity")).plugin_options(options).open();
}

fn main() {}
