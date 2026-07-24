use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, header};
use sqlx::SqlitePool;

use crate::{
    domain::{SyncUser, UserSession},
    integrations::gmail::GoogleTokenResponse,
};

const SESSION_COOKIE: &str = "inbox_session";

pub async fn upsert_user(
    pool: &SqlitePool,
    email: &str,
    token: &GoogleTokenResponse,
) -> Result<i64, sqlx::Error> {
    let token_expires_at = token.expires_in.map(|seconds| unix_now() + seconds);
    sqlx::query(
        "INSERT INTO users (google_email, access_token, refresh_token, expires_in, token_expires_at, gmail_reauth_required, updated_at)
         VALUES (?, ?, ?, ?, ?, 0, CURRENT_TIMESTAMP)
         ON CONFLICT(google_email) DO UPDATE SET
           access_token = excluded.access_token,
           refresh_token = COALESCE(excluded.refresh_token, users.refresh_token),
           expires_in = excluded.expires_in,
           token_expires_at = excluded.token_expires_at,
           gmail_reauth_required = 0,
           updated_at = CURRENT_TIMESTAMP",
    )
    .bind(email)
    .bind(&token.access_token)
    .bind(&token.refresh_token)
    .bind(token.expires_in)
    .bind(token_expires_at)
    .execute(pool)
    .await?;

    let (user_id,): (i64,) = sqlx::query_as("SELECT id FROM users WHERE google_email = ?")
        .bind(email)
        .fetch_one(pool)
        .await?;
    Ok(user_id)
}

pub async fn load_sync_user(pool: &SqlitePool, user_id: i64) -> Result<SyncUser, sqlx::Error> {
    let (
        user_id,
        email,
        access_token,
        refresh_token,
        token_expires_at,
        gmail_history_id,
        gmail_reauth_required,
    ): (i64, String, String, Option<String>, Option<i64>, Option<String>, i64) =
        sqlx::query_as(
            "SELECT id, google_email, access_token, refresh_token, token_expires_at, gmail_history_id, gmail_reauth_required
             FROM users
             WHERE id = ?",
        )
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    Ok(SyncUser {
        user_id,
        email,
        access_token,
        refresh_token,
        token_expires_at,
        gmail_history_id,
        gmail_reauth_required: gmail_reauth_required != 0,
    })
}

pub async fn mark_gmail_reauth_required(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE users SET gmail_reauth_required = 1, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn create_session(pool: &SqlitePool, user_id: i64) -> Result<String, sqlx::Error> {
    let token = random_token(pool).await?;
    sqlx::query("INSERT INTO sessions (token, user_id) VALUES (?, ?)")
        .bind(&token)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(token)
}

pub async fn require_user_session(
    headers: &HeaderMap,
    pool: &SqlitePool,
) -> Result<Option<UserSession>, sqlx::Error> {
    let Some(token) = session_token_from_headers(headers) else {
        return Ok(None);
    };
    let row = sqlx::query_as::<_, (i64, String, Option<String>)>(
        "SELECT users.id, users.google_email, users.openrouter_key FROM sessions JOIN users ON users.id = sessions.user_id WHERE sessions.token = ?",
    )
    .bind(token)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(user_id, email, openrouter_key)| UserSession {
        user_id,
        email,
        openrouter_key,
    }))
}

pub async fn random_token(pool: &SqlitePool) -> Result<String, sqlx::Error> {
    let (token,): (String,) = sqlx::query_as("SELECT lower(hex(randomblob(32)))")
        .fetch_one(pool)
        .await?;
    Ok(token)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_secs()
        .try_into()
        .expect("current Unix timestamp should fit i64")
}

fn session_token_from_headers(headers: &HeaderMap) -> Option<&str> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        let trimmed = part.trim();
        trimmed
            .strip_prefix(SESSION_COOKIE)
            .and_then(|value| value.strip_prefix('='))
    })
}
