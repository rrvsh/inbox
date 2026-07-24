use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect, Response},
};

use super::forms::InboxQuery;
use crate::{
    app::AppState,
    error::AppError,
    repository::require_user_session,
    services::client,
    templates::{InboxTableTemplate, InboxTemplate, SyncButtonTemplate, render},
};

pub async fn inbox(
    headers: HeaderMap,
    State(state): State<AppState>,
    Query(query): Query<InboxQuery>,
) -> Result<Response, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let selected_filter = query.state.as_deref().unwrap_or("NeedsAction");
    let selected_state = Some(selected_filter).filter(|value| *value != "all");
    let threads = client::inbox_threads(&state.pool, session.user_id, selected_state).await?;

    render(InboxTemplate {
        threads,
        filters: client::filters(selected_filter),
        user_email: session.email,
        syncing: client::sync_button_is_syncing(&state.pool, session.user_id).await?,
    })
}

pub async fn inbox_table(
    headers: HeaderMap,
    State(state): State<AppState>,
    Query(query): Query<InboxQuery>,
) -> Result<Response, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let selected_filter = query.state.as_deref().unwrap_or("NeedsAction");
    let selected_state = Some(selected_filter).filter(|value| *value != "all");
    let threads = client::inbox_threads(&state.pool, session.user_id, selected_state).await?;

    render(InboxTableTemplate { threads })
}

pub async fn sync_button(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    render(SyncButtonTemplate {
        syncing: client::sync_button_is_syncing(&state.pool, session.user_id).await?,
    })
}
