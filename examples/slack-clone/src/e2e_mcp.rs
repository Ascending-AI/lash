//! H6's transport crash cut. Compiled only into explicitly selected fixtures.
//! The synced marker proves this peer entered the actual badge handler; it
//! records no engine journal commands and contains no credentials.
use std::io::Write as _;

pub async fn badge_entered() -> Result<(), rmcp::model::ErrorData> {
    let Some(root) = std::env::var_os("SLACK_CLONE_E2E_MCP_GATE_DIR") else {
        return Ok(());
    };
    let root = std::path::PathBuf::from(root);
    let record = || -> std::io::Result<bool> {
        std::fs::create_dir_all(&root)?;
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(root.join("badge-entered"))
        {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                file.sync_all()?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    };
    if record().map_err(|error| rmcp::model::ErrorData::internal_error(error.to_string(), None))? {
        // Only a controller kill/restart releases this first incarnation.
        std::future::pending::<()>().await;
    }
    Ok(())
}
