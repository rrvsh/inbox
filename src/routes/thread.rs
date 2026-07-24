use axum::{
    extract::{Form, Path, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect, Response},
};

use super::forms::UserSubmission;
use crate::{
    app::AppState,
    domain::{ActionLogRow, MessageRow, ThreadRow, ThreadView, UserFormRow},
    error::AppError,
    repository::{
        log_thread_event, require_user_session, set_thread_state, submit_active_forms, thread_chats,
    },
    services::workflow,
    templates::{ThreadTemplate, render},
};

pub async fn thread(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login").into_response());
    };

    let thread = sqlx::query_as::<_, ThreadRow>(
        "SELECT id, subject, from_display, state, latest_action, waiting_until FROM threads WHERE id = ? AND user_id = ?",
    )
    .bind(id)
    .bind(session.user_id)
    .fetch_one(&state.pool)
    .await?;
    let messages = sqlx::query_as::<_, MessageRow>(
        "SELECT sender, body, sent_at FROM messages WHERE thread_id = ? ORDER BY sent_at ASC",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    let form = sqlx::query_as::<_, UserFormRow>(
        "SELECT form_type, title, body FROM user_forms WHERE thread_id = ? AND status = 'active' ORDER BY id DESC LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;
    let logs = sqlx::query_as::<_, ActionLogRow>(
        "SELECT actor, message, created_at FROM action_logs WHERE thread_id = ? ORDER BY id ASC",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    let chats = thread_chats(&state.pool, id).await?;

    render(ThreadTemplate {
        thread: ThreadView::from(thread),
        messages,
        form,
        logs,
        chats,
        user_email: session.email,
    })
}

pub async fn submit_thread_form(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(submission): Form<UserSubmission>,
) -> Result<Redirect, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login"));
    };

    let thread_exists =
        sqlx::query_as::<_, (i64,)>("SELECT id FROM threads WHERE id = ? AND user_id = ?")
            .bind(id)
            .bind(session.user_id)
            .fetch_optional(&state.pool)
            .await?
            .is_some();
    if !thread_exists {
        return Ok(Redirect::to("/"));
    }

    let form = sqlx::query_as::<_, (String,)>(
        "SELECT form_type FROM user_forms WHERE thread_id = ? AND status = 'active' ORDER BY id DESC LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;

    let response_text = submission
        .comment
        .as_deref()
        .or(submission.response.as_deref())
        .or(submission.draft_body.as_deref())
        .unwrap_or("");
    log_thread_event(
        &state.pool,
        id,
        "user",
        &format!("Submitted {}: {}", submission.action, response_text),
    )
    .await?;

    match (
        form.as_ref().map(|(form_type,)| form_type.as_str()),
        submission.action.as_str(),
    ) {
        (Some("AskToResolve"), "resolve") => {
            submit_active_forms(&state.pool, id).await?;
            set_thread_state(&state.pool, id, "Resolved", "User confirmed resolved", None).await?;
            log_thread_event(&state.pool, id, "system", "State changed to Resolved").await?;
            return Ok(Redirect::to("/"));
        }
        (Some("DraftReply"), "comment") => {
            workflow::handle_draft_revision(&state, &session, id, &submission).await?;
        }
        (Some("DraftReply"), "resolve") => {
            submit_active_forms(&state.pool, id).await?;
            set_thread_state(&state.pool, id, "Resolved", "User resolved thread", None).await?;
            log_thread_event(&state.pool, id, "system", "State changed to Resolved").await?;
            return Ok(Redirect::to("/"));
        }
        (Some("AskToResolve" | "AskUser"), "comment") => {
            workflow::handle_classifier_comment(&state, &session, id, &submission).await?;
        }
        _ => {
            submit_active_forms(&state.pool, id).await?;
            set_thread_state(&state.pool, id, "Processing", "Classifier running", None).await?;
            workflow::run_classifier(&state, &session, id).await?;
        }
    }

    Ok(Redirect::to(&format!("/threads/{id}")))
}
