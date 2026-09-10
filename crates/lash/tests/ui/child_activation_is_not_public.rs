fn assert_child_activation_is_not_public(session: &lash::LashSession) {
    let _ = session
        .admin()
        .children()
        .activate_managed_session("another-session");
}

fn main() {}
