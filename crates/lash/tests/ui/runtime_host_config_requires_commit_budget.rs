fn host(backend: std::sync::Arc<dyn lash::Backend>) {
    let _host = lash::durability::RuntimeHostConfig::new(backend);
}

fn main() {}
