//! FIG-6273: the registry is a type. A process component's registrar offers
//! only the contributions the contribution table admits in a process, so a
//! process recipe that declares a turn hook does not compile.
use lash::plugins::PluginRegistrar;
use lash::plugins::registries::Process;

fn main() {
    let mut reg = PluginRegistrar::<(), Process>::for_process("component");
    let _ = reg
        .turn()
        .before(lash::hook_key!("before"), |_, _| unreachable!());
}
