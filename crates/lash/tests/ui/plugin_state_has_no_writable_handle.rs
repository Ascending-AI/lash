// K10: a plugin reads its namespace through a read-only view. No handle a
// plugin retains writes; a change is a command returned with a recorded
// result.
use lash::plugins::PluginStateView;

fn write(view: &PluginStateView) {
    let _ = view.set("key", ());
    let _ = view.remove("key");
}

fn main() {}
