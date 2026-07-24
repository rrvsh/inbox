use sqlx::SqlitePool;

use crate::domain::{ThreadRow, ThreadView};

pub async fn user_has_threads(pool: &SqlitePool, user_id: i64) -> Result<bool, sqlx::Error> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM threads WHERE user_id = ?")
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    Ok(count > 0)
}

pub async fn fetch_thread_views(
    pool: &SqlitePool,
    user_id: i64,
    selected_state: Option<&str>,
) -> Result<Vec<ThreadView>, sqlx::Error> {
    let thread_rows = if let Some(selected_state) = selected_state {
        sqlx::query_as::<_, ThreadRow>(
            "SELECT id, subject, from_display, state, latest_action, waiting_until, last_message_at, unread FROM threads WHERE user_id = ? AND state = ? ORDER BY updated_at DESC",
        )
        .bind(user_id)
        .bind(selected_state)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, ThreadRow>(
            "SELECT id, subject, from_display, state, latest_action, waiting_until, last_message_at, unread FROM threads WHERE user_id = ? ORDER BY updated_at DESC",
        )
        .bind(user_id)
        .fetch_all(pool)
        .await?
    };
    Ok(thread_rows.into_iter().map(ThreadView::from).collect())
}
