use super::*;

impl Store {
    pub(super) async fn record_turn_park_impl(
        &self,
        park: &lash_core_execution::store::TurnParkWrite,
    ) -> Result<lash_core_execution::store::TurnPark, StoreError> {
        self.bind_session(&park.session_id)?;
        let session_id = park.session_id.clone();
        let turn_id = park.turn_id.as_str().to_string();
        let reason_code = park.reason.code().as_str().to_string();
        let reason_json = serde_json::to_string(&park.reason).map_err(|error| {
            StoreError::RecordEncodingFailed {
                record_kind: "TurnPark".to_string(),
                message: error.to_string(),
            }
        })?;
        let reason = park.reason.clone();
        let park_executable_generation = reason
            .retired_executable_generation_key()
            .map(str::to_string);
        let build_generation = park.build_generation.clone();
        let park_build_generation = park
            .build_generation
            .as_ref()
            .map(|generation| generation.as_str().to_string());
        let at_ms = i64::try_from(park.at_ms).map_err(|_| {
            StoreError::Backend(format!(
                "turn park instant {} exceeds the stored range",
                park.at_ms
            ))
        })?;
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core_execution::store::TurnPark, StoreError> = (|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    let turn_parks = &crate::turn_ingress::turn_ingress_sql().turn_parks;
                    let existing: Option<(String, i64, i64, i64)> = tx
                        .query_row(
                            turn_parks.select_by_session.sql(),
                            params![session_id.as_str()],
                            |row| {
                                Ok((
                                    row.get::<_, String>(1)?,
                                    row.get::<_, i64>(2)?,
                                    row.get::<_, i64>(5)?,
                                    row.get::<_, i64>(7)?,
                                ))
                            },
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    match existing {
                        Some((stored_turn_id, park_id, since_ms, attempts))
                            if stored_turn_id == turn_id =>
                        {
                            // A same-turn re-park keeps `park_id` and
                            // `since_ms`, refreshes the reason and
                            // `last_refused_ms`, and counts the refusal — no
                            // feed event.
                            tx.execute(
                                turn_parks.update_same_turn.sql(),
                                params![
                                    session_id.as_str(),
                                    turn_id,
                                    reason_code,
                                    reason_json,
                                    at_ms,
                                    park_executable_generation,
                                    park_build_generation
                                ],
                            )
                            .map_err(sqlite_error)?;
                            return Ok(lash_core_execution::store::TurnPark {
                                session_id: session_id.clone(),
                                turn_id: turn_id.clone().into(),
                                reason,
                                park_id: lash_core_execution::store::ParkId::from_feed_sequence(
                                    u64::try_from(park_id).unwrap_or_default(),
                                ),
                                since_ms: u64::try_from(since_ms).unwrap_or_default(),
                                last_refused_ms: u64::try_from(at_ms).unwrap_or_default(),
                                attempts: u32::try_from(attempts.saturating_add(1))
                                    .unwrap_or(u32::MAX),
                                build_generation,
                            });
                        }
                        Some((superseded_turn_id, superseded_park_id, ..)) => {
                            // A different turn's park supersedes the stored
                            // one: close it on the feed, then open the new
                            // park.
                            let deleted: Option<(String, i64)> = tx
                                .query_row(
                                    turn_parks.delete_for_supersede_returning.sql(),
                                    params![session_id.as_str(), turn_id],
                                    |row| Ok((row.get(0)?, row.get(1)?)),
                                )
                                .optional()
                                .map_err(sqlite_error)?;
                            if deleted.is_none() {
                                return Err(StoreError::Backend(format!(
                                    "turn park supersede of `{superseded_turn_id}` in session \
                                     `{session_id}` deleted no row"
                                )));
                            }
                            crate::persistence::turn_park_feed::log_turn_park_closed_conn(
                                tx,
                                &session_id,
                                &superseded_turn_id,
                                superseded_park_id,
                                &lash_core_execution::store::ParkEventKind::Unparked {
                                    cause: lash_core_execution::store::UnparkCause::Superseded,
                                },
                                at_ms,
                            )?;
                        }
                        None => {}
                    }
                    let park_id = crate::persistence::turn_park_feed::log_turn_parked_conn(
                        tx,
                        &session_id,
                        &turn_id,
                        &reason,
                        at_ms,
                        park_build_generation.as_deref(),
                    )?;
                    tx.execute(
                        turn_parks.insert.sql(),
                        params![
                            session_id.as_str(),
                            turn_id,
                            park_id,
                            reason_code,
                            reason_json,
                            at_ms,
                            at_ms,
                            1,
                            park_executable_generation,
                            park_build_generation
                        ],
                    )
                    .map_err(sqlite_error)?;
                    Ok(lash_core_execution::store::TurnPark {
                        session_id: session_id.clone(),
                        turn_id: turn_id.clone().into(),
                        reason,
                        park_id: lash_core_execution::store::ParkId::from_feed_sequence(
                            u64::try_from(park_id).unwrap_or_default(),
                        ),
                        since_ms: u64::try_from(at_ms).unwrap_or_default(),
                        last_refused_ms: u64::try_from(at_ms).unwrap_or_default(),
                        attempts: 1,
                        build_generation,
                    })
                })(
                );
                Ok(match outcome {
                    Ok(park) => TxOutcome::Commit(Ok(park)),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    pub(super) async fn load_turn_park_impl(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<lash_core_execution::store::TurnPark>, StoreError> {
        let session = session_id.as_str().to_string();
        let row: Option<(String, i64, String, String, i64, i64, i64, Option<String>)> = self
            .conn
            .call(move |conn| {
                conn.query_row(
                    crate::turn_ingress::turn_ingress_sql()
                        .turn_parks
                        .select_by_session
                        .sql(),
                    params![session],
                    |row| {
                        Ok((
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                        ))
                    },
                )
                .optional()
            })
            .await
            .map_err(sqlite_error)?;
        row.map(
            |(
                turn_id,
                park_id,
                reason_code,
                reason_json,
                since_ms,
                last_refused_ms,
                attempts,
                build_generation,
            )| {
                lash_core_execution::store::TurnPark::decode(
                    session_id.clone(),
                    turn_id.into(),
                    lash_core_execution::store::ParkId::from_feed_sequence(
                        u64::try_from(park_id).unwrap_or_default(),
                    ),
                    &reason_code,
                    &reason_json,
                    u64::try_from(since_ms).unwrap_or_default(),
                    u64::try_from(last_refused_ms).unwrap_or_default(),
                    u32::try_from(attempts).unwrap_or(u32::MAX),
                    build_generation.as_deref(),
                )
            },
        )
        .transpose()
    }
}
