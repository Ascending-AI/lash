fn cannot_mutate_session_through_read_view(view: lash::persistence::SessionReadView) {
    view.commit_runtime_state();
}

fn main() {}
