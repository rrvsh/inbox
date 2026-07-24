use sqlx::SqlitePool;

use crate::domain::ChatRow;

pub async fn thread_chats(pool: &SqlitePool, thread_id: i64) -> Result<Vec<ChatRow>, sqlx::Error> {
    sqlx::query_as::<_, ChatRow>(
        "SELECT role, message FROM thread_chats WHERE thread_id = ? ORDER BY id ASC",
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await
}

pub async fn insert_thread_chat(
    pool: &SqlitePool,
    thread_id: i64,
    role: &str,
    message: &str,
) -> Result<(), sqlx::Error> {
    eprintln!("db: insert_thread_chat thread_id={thread_id} role={role} message={message}");
    sqlx::query(
        "INSERT INTO thread_chats (thread_id, role, message, created_at) VALUES (?, ?, ?, CURRENT_TIMESTAMP)",
    )
    .bind(thread_id)
    .bind(role)
    .bind(message)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn submit_active_forms(pool: &SqlitePool, thread_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE user_forms SET status = 'submitted' WHERE thread_id = ? AND status = 'active'",
    )
    .bind(thread_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn create_user_form(
    pool: &SqlitePool,
    thread_id: i64,
    form_type: &str,
    title: &str,
    body: &str,
) -> Result<(), sqlx::Error> {
    eprintln!(
        "db: create_user_form thread_id={thread_id} form_type={form_type} title={title} body_bytes={}",
        body.len()
    );
    sqlx::query(
        "UPDATE user_forms SET status = 'superseded' WHERE thread_id = ? AND status = 'active'",
    )
    .bind(thread_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO user_forms (thread_id, form_type, title, body, status, created_at) VALUES (?, ?, ?, ?, 'active', CURRENT_TIMESTAMP)",
    )
    .bind(thread_id)
    .bind(form_type)
    .bind(title)
    .bind(body)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_thread_state(
    pool: &SqlitePool,
    thread_id: i64,
    state: &str,
    latest_action: &str,
    waiting_until: Option<&str>,
) -> Result<(), sqlx::Error> {
    eprintln!(
        "db: set_thread_state thread_id={thread_id} state={state} latest_action={latest_action} waiting_until={}",
        waiting_until.unwrap_or("")
    );
    sqlx::query(
        "UPDATE threads SET state = ?, latest_action = ?, waiting_until = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(state)
    .bind(latest_action)
    .bind(waiting_until)
    .bind(thread_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn log_thread_event(
    pool: &SqlitePool,
    thread_id: i64,
    actor: &str,
    message: &str,
) -> Result<(), sqlx::Error> {
    eprintln!("db: log_thread_event thread_id={thread_id} actor={actor} message={message}");
    sqlx::query(
        "INSERT INTO action_logs (thread_id, actor, message, created_at) VALUES (?, ?, ?, CURRENT_TIMESTAMP)",
    )
    .bind(thread_id)
    .bind(actor)
    .bind(message)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn log_thread_state_counts(
    pool: &SqlitePool,
    user_id: i64,
    label: &str,
) -> Result<(), sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, i64)>(
        "SELECT state, COUNT(*) FROM threads WHERE user_id = ? GROUP BY state ORDER BY state",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let summary = rows
        .into_iter()
        .map(|(state, count)| format!("{state}={count}"))
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("thread_counts: label={label} user_id={user_id} {summary}");
    Ok(())
}
