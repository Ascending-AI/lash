//! Every rendered statement set this crate owns renders, for every layout.
//!
//! The PostgreSQL twin of this test (`rendered_statement_sets_tests.rs` there)
//! explains why it exists: rendering is a `LazyLock` behind each `*_sql()`
//! accessor, so a statement the renderer refuses is a panic at first use and
//! nowhere earlier. SQLite renders one set per deployment layout, so this side
//! has to force every layout as well as every set — a table a layout does not
//! place is a render refusal, which is the point of the layout and also the
//! way a new statement can fail for one connection shape and not another.

use crate::scope_fence::Schema;

#[test]
fn every_rendered_statement_set_renders_for_every_layout() {
    let _ = crate::artifact_store::artifact_sql();
    let _ = crate::attachments::attachment_sql();
    let _ = crate::session_sql::session_sql();
    let _ = crate::turn_ingress::turn_ingress_sql();
    let _ = crate::turn_ingress::tool_intent_sql();
    let _ = crate::process_registry::sql::process_sql();
    let _ = crate::process_registry::sql::attached_process_sql();
    for schema in Schema::ALL {
        let _ = crate::scope_fence::fence_sql(schema);
        let _ = crate::await_event::wait_sql(schema);
        let _ = crate::effect_replay::effect_sql(schema);
        let _ = crate::turn_ingress::closure_participant_sql(schema);
    }
}
