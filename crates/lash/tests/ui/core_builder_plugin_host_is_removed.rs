use lash::{LashCoreBuilder, plugins::PluginHost};

fn override_host(builder: LashCoreBuilder, host: PluginHost) {
    let _ = builder.advanced().plugin_host(host);
}

fn main() {}
