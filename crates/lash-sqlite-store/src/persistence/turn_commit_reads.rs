//! Reads over the durable turn-commit records: whether a turn's final commit
//! landed, and the input applications every commit settled.

use super::*;

impl Store {
    pub(super) async fn read_turn_is_committed(
        &self,
        address: &lash_core_execution::facade_support::TurnAddress,
    ) -> Result<bool, StoreError> {
        let session_id = address.session_id.clone();
        let operation_key =
            lash_core_execution::OperationId::turn(&address.session_id, &address.turn_id, "final")
                .storage_key()?;
        self.conn
            .call(move |conn| {
                conn.query_row(
                    crate::session_sql::session_sql()
                        .turn_commits
                        .exists_for_turn
                        .sql(),
                    params![session_id.as_str(), operation_key],
                    |row| row.get::<_, bool>(0),
                )
            })
            .await
            .map_err(sqlite_error)
    }

    pub(super) async fn read_turn_input_applications(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::TurnInputApplication>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        self.conn
            .call(move |conn| {
                let outcome = (|| {
                    let mut stmt = conn
                        .prepare(
                            crate::session_sql::session_sql()
                                .turn_commits
                                .select_all_for_session
                                .sql(),
                        )
                        .map_err(sqlite_error)?;
                    let rows = stmt
                        .query_map(params![session_id.as_str()], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(sqlite_error)?;
                    let mut commits = Vec::new();
                    for row in rows {
                        let (turn_id, result_json) = row.map_err(sqlite_error)?;
                        let result = lash_core_execution::store::decode_runtime_commit_receipt(
                            &session_id,
                            &turn_id,
                            &result_json,
                        )?;
                        commits.push((
                            result.head_revision,
                            turn_id,
                            result.turn_input_applications,
                        ));
                    }
                    commits.sort_by(|left, right| {
                        (left.0, left.1.as_str()).cmp(&(right.0, right.1.as_str()))
                    });
                    Ok(commits
                        .into_iter()
                        .flat_map(|(_, _, applications)| applications)
                        .collect())
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }
}
