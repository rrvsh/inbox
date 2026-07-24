use axum::{
    extract::{Form, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect, Response},
};

use super::forms::SettingsSubmission;
use crate::{
    app::AppState,
    error::AppError,
    repository::require_user_session,
    templates::{SettingsTemplate, render},
};

pub async fn settings(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login").into_response());
    };

    render(SettingsTemplate {
        user_email: session.email,
        openrouter_key: session.openrouter_key.unwrap_or_default(),
    })
}

pub async fn save_settings(
    headers: HeaderMap,
    State(state): State<AppState>,
    Form(submission): Form<SettingsSubmission>,
) -> Result<Redirect, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login"));
    };

    sqlx::query("UPDATE users SET openrouter_key = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?")
        .bind(submission.openrouter_key.trim())
        .bind(session.user_id)
        .execute(&state.pool)
        .await?;

    Ok(Redirect::to("/settings"))
}

pub async fn reset_synced_data(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Redirect, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login"));
    };

    sqlx::query(
        "DELETE FROM action_logs WHERE thread_id IN (SELECT id FROM threads WHERE user_id = ?)",
    )
    .bind(session.user_id)
    .execute(&state.pool)
    .await?;
    sqlx::query(
        "DELETE FROM user_forms WHERE thread_id IN (SELECT id FROM threads WHERE user_id = ?)",
    )
    .bind(session.user_id)
    .execute(&state.pool)
    .await?;
    sqlx::query(
        "DELETE FROM messages WHERE thread_id IN (SELECT id FROM threads WHERE user_id = ?)",
    )
    .bind(session.user_id)
    .execute(&state.pool)
    .await?;
    sqlx::query(
        "DELETE FROM thread_chats WHERE thread_id IN (SELECT id FROM threads WHERE user_id = ?)",
    )
    .bind(session.user_id)
    .execute(&state.pool)
    .await?;
    sqlx::query("DELETE FROM threads WHERE user_id = ?")
        .bind(session.user_id)
        .execute(&state.pool)
        .await?;
    sqlx::query("DELETE FROM sync_jobs WHERE user_id = ?")
        .bind(session.user_id)
        .execute(&state.pool)
        .await?;

    Ok(Redirect::to("/settings"))
}
