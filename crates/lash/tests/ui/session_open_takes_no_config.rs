// FIG-4112: an open takes no session config. Model, prompt, parent and plugin
// options are creation config, stated once in the `SessionCreation` passed to
// `create`; the open builder cannot carry them.

fn open_takes_no_model(core: lash::LashCore, model: lash::ModelSpec) {
    let _ = core
        .session("stated-at-open")
        .session_spec(lash::SessionSpec::new().model(model))
        .open();
}

fn open_takes_no_prompt(core: lash::LashCore) {
    use lash::PromptLayerSink as _;

    let _ = core.session("stated-at-open").instructions("prompt").open();
}

fn open_takes_no_parent(core: lash::LashCore) {
    let _ = core.session("stated-at-open").parent("parent").open();
}

fn open_takes_no_plugin_options(core: lash::LashCore, options: lash::plugins::PluginOptions) {
    let _ = core.session("stated-at-open").plugin_options(options).open();
}

fn main() {}
