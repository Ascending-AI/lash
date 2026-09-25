fn host(backend: lash::Backend) {
    let _host = lash::durability::RuntimeHostConfig::new(backend);
}

fn main() {}
