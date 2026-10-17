use super::*;
use crate::plugins::{PluginOperation, PluginQuery, SessionParam};
struct Query;
impl PluginOperation for Query {
    const NAME: &'static str = "accept.query";
    const DESCRIPTION: &'static str = "Plugin facade acceptance probe";
    const SESSION_PARAM: SessionParam = SessionParam::Required;
    type Args = String;
    type Output = String;
    type Error = String;
    const ERROR_TYPE: &'static str = Self::NAME;
    const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
    fn error_class(_: &Self::Error) -> lash_protocol_types::PluginFailureClass {
        lash_protocol_types::PluginFailureClass::Terminal
    }
}
impl PluginQuery for Query {}

/// A handle reopened on a session whose turn the core's node served answers
/// a plugin query from the plugin view that turn's run published, and the
/// query appends nothing to the session's history (FIG-5344). A session that
/// never ran is still refused as not published
/// (`admin_reads_without_runtime`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plugin_query_on_a_reopened_served_session_answers_from_its_published_view() -> Result<()>
{
    let spec = lash_core::facade_support::PluginSpec::new().with_plugin_query_typed::<Query, _, _>(
        |ctx, args| async move {
            Ok(format!(
                "{args} from {}",
                ctx.session_id.expect("session").as_str()
            ))
        },
    );
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .plugin(Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("accept"),
        spec,
    )))
    .build(crate::tests::host::runtime_lease_owner())?;
    let id = crate::SessionId::parse("query-after-served-turn").expect("nonblank host identity");
    created_session(&core, id.clone())
        .await
        .durable()
        .await?
        .send(TurnInput::text("serve one turn"))
        .output()
        .await?;
    let head = || async {
        lash_core::SessionCommitStore::load_session_head_meta(core.store_factory.as_ref(), &id)
            .await
            .expect("head read")
            .expect("persisted head")
            .head_revision
    };
    let served = head().await;
    let reopened = core.session(id.clone()).open().await?;
    let answer = reopened
        .plugin_operations()
        .query::<Query>("asked".into())
        .await?;
    assert_eq!(answer, "asked from query-after-served-turn");
    assert_eq!(head().await, served, "a query appends no history");
    drop(reopened);
    core.shutdown().await?;
    Ok(())
}
