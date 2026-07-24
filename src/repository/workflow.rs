use sqlx::SqlitePool;

use crate::domain::UserSession;

pub async fn next_unprocessed_thread_id(pool: &SqlitePool) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM threads WHERE state = 'Unprocessed' ORDER BY updated_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}

pub async fn user_session_for_thread(
    pool: &SqlitePool,
    thread_id: i64,
) -> Result<UserSession, sqlx::Error> {
    let (user_id, email, openrouter_key): (i64, String, Option<String>) = sqlx::query_as(
        "SELECT users.id, users.google_email, users.openrouter_key
             FROM threads
             JOIN users ON users.id = threads.user_id
             WHERE threads.id = ?",
    )
    .bind(thread_id)
    .fetch_one(pool)
    .await?;
    Ok(UserSession {
        user_id,
        email,
        openrouter_key,
    })
}

pub async fn claim_unprocessed_thread(
    pool: &SqlitePool,
    thread_id: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE threads SET state = 'Processing', latest_action = 'Classifier running', waiting_until = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND state = 'Unprocessed'",
    )
    .bind(thread_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}
