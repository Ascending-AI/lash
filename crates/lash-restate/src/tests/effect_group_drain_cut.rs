type Hook = (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);
static HOOKS: std::sync::LazyLock<Mutex<BTreeMap<(String, &'static str), Hook>>> =
    std::sync::LazyLock::new(|| Mutex::new(BTreeMap::new()));

use super::*;

pub(crate) fn arm(key: &str, cut: &'static str) -> Hook {
    let hook = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((key.to_owned(), cut), hook.clone());
    hook
}

pub(super) async fn pause(
    ctx: &ObjectContext<'_>,
    key: &str,
    cut: &'static str,
) -> HandlerResult<()> {
    let hook = HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(key.to_owned(), cut));
    if let Some((reached, release)) = hook {
        ctx.run(|| async { Ok(()) })
            .name(format!("drain-cut-{cut}"))
            .await?;
        reached.notify_one();
        release.notified().await;
    }
    Ok(())
}
