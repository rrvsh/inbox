use sqlx::SqlitePool;

use crate::integrations::gmail::{GmailThreadResponse, extract_body_text, header_value};

pub async fn upsert_gmail_thread(
    pool: &SqlitePool,
    user_id: i64,
    gmail_thread: &GmailThreadResponse,
) -> Result<i64, sqlx::Error> {
    eprintln!(
        "gmail_thread_upsert: user_id={user_id} gmail_thread_id={} messages={}",
        gmail_thread.id,
        gmail_thread.messages.len()
    );
    let first_message = gmail_thread.messages.first();
    let last_message = gmail_thread.messages.last();
    let subject = first_message
        .and_then(|message| header_value(&message.payload.headers, "Subject"))
        .unwrap_or("(no subject)");
    let from_display = last_message
        .and_then(|message| header_value(&message.payload.headers, "From"))
        .unwrap_or("(unknown sender)");
    let last_message_at = last_message.and_then(|message| message.internal_date.as_deref());

    sqlx::query(
        "INSERT INTO threads (user_id, gmail_thread_id, subject, from_display, state, latest_action, waiting_until, updated_at, snippet, last_message_at)
         VALUES (?, ?, ?, ?, 'Unprocessed', 'Synced from Gmail', NULL, CURRENT_TIMESTAMP, ?, ?)
         ON CONFLICT(user_id, gmail_thread_id) DO UPDATE SET
           subject = excluded.subject,
           from_display = excluded.from_display,
           state = CASE
             WHEN threads.last_message_at IS NOT excluded.last_message_at THEN 'Unprocessed'
             ELSE threads.state
           END,
           latest_action = CASE
             WHEN threads.last_message_at IS NOT excluded.last_message_at THEN 'Synced from Gmail'
             ELSE threads.latest_action
           END,
           updated_at = CURRENT_TIMESTAMP,
           snippet = excluded.snippet,
           last_message_at = excluded.last_message_at",
    )
    .bind(user_id)
    .bind(&gmail_thread.id)
    .bind(subject)
    .bind(from_display)
    .bind(gmail_thread.snippet.as_deref())
    .bind(last_message_at)
    .execute(pool)
    .await?;

    let (thread_id,): (i64,) =
        sqlx::query_as("SELECT id FROM threads WHERE user_id = ? AND gmail_thread_id = ?")
            .bind(user_id)
            .bind(&gmail_thread.id)
            .fetch_one(pool)
            .await?;

    sqlx::query("DELETE FROM messages WHERE thread_id = ?")
        .bind(thread_id)
        .execute(pool)
        .await?;

    for message in &gmail_thread.messages {
        let sender = header_value(&message.payload.headers, "From").unwrap_or("(unknown sender)");
        let sent_at = message.internal_date.as_deref().unwrap_or("");
        let body = extract_body_text(&message.payload).unwrap_or_default();
        sqlx::query(
            "INSERT INTO messages (thread_id, gmail_message_id, sender, body, sent_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(thread_id)
        .bind(&message.id)
        .bind(sender)
        .bind(body)
        .bind(sent_at)
        .execute(pool)
        .await?;
    }

    sqlx::query(
        "INSERT INTO action_logs (thread_id, actor, message, created_at) VALUES (?, 'system', 'Synced latest Gmail thread data', CURRENT_TIMESTAMP)",
    )
    .bind(thread_id)
    .execute(pool)
    .await?;

    Ok(thread_id)
}
