#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CronSessionDisposition {
    Live,
    Retired,
    Unknown,
}

impl CronSessionDisposition {
    fn journal_value(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Retired => "retired",
            Self::Unknown => "unknown",
        }
    }

    fn from_journal_value(value: &str) -> HandlerResult<Self> {
        match value {
            "live" => Ok(Self::Live),
            "retired" => Ok(Self::Retired),
            "unknown" => Ok(Self::Unknown),
            _ => Err(TerminalError::new(format!(
                "invalid journaled cron session disposition `{value}`"
            ))
            .into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum CronTick {
    Cancel { trace: Value },
    Run,
}

fn cron_tick_decision(
    disposition: CronSessionDisposition,
    state: &WorkbenchCronState,
    job_key: &str,
) -> CronTick {
    let (decision_basis, session_state, reason) = match disposition {
        CronSessionDisposition::Live => return CronTick::Run,
        CronSessionDisposition::Retired => {
            ("deleted_session_tombstone", "retired", "session_retired")
        }
        CronSessionDisposition::Unknown => {
            ("session_store_meta_absent", "unknown", "session_absent")
        }
    };
    CronTick::Cancel {
        trace: json!({
            "job_key": job_key,
            "job_session_id": state.request.session_id,
            "decision_basis": decision_basis,
            "session_state": session_state,
            "reason": reason,
        }),
    }
}

async fn cron_session_disposition(
    core: &lash::LashCore,
    session_id: &str,
) -> Result<CronSessionDisposition, HandlerError> {
    if core
        .session_was_deleted(session_id)
        .await
        .map_err(classified_embed_handler_error)?
    {
        return Ok(CronSessionDisposition::Retired);
    }
    if core
        .session_exists(session_id)
        .await
        .map_err(classified_embed_handler_error)?
    {
        Ok(CronSessionDisposition::Live)
    } else {
        Ok(CronSessionDisposition::Unknown)
    }
}
