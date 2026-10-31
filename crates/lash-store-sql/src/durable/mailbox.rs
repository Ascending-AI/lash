//! The bounded actor-mail prefix shared by cold and retained owner reads.

/// Add the actor `?1`'s mailbox page to a named owner statement. `budget` and
/// `rows` are numbered placeholders; `select` supplies the backend's owner
/// projection and JSON aggregation of `mail_page`.
#[macro_export]
macro_rules! mailbox_query {
    ($budget:literal, $rows:literal, $select:literal) => {
        concat!(
            "WITH mail_candidates AS (
                SELECT seq, kind, appended_at_ms, octet_length(body) AS body_bytes
                FROM actor_mail WHERE actor_key = ?1
                  AND seq > (SELECT acked_seq FROM actors WHERE actor_key = ?1)
                ORDER BY seq LIMIT ",
            $rows,
            "
             ), mail_prefix AS (
                SELECT *, SUM(body_bytes) OVER (ORDER BY seq ROWS UNBOUNDED PRECEDING) AS bytes
                FROM mail_candidates
             ), mail_page AS (
                SELECT p.seq, p.kind, CASE WHEN p.bytes <= ",
            $budget,
            " THEN m.body END AS body,
                       p.appended_at_ms, p.body_bytes
                FROM mail_prefix p JOIN actor_mail m ON m.actor_key = ?1 AND m.seq = p.seq
                WHERE p.bytes - p.body_bytes <= ",
            $budget,
            " ORDER BY p.seq
             ) ",
            $select
        )
    };
}
