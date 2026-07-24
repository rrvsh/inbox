use askama::Template;
use axum::{
    Router,
    extract::{Form, Path, Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE};
use reqwest::Url;
use sqlx::{FromRow, SqlitePool, sqlite::SqliteConnectOptions};
use std::{
    collections::HashSet,
    env,
    error::Error,
    fmt::Write,
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;

const SESSION_COOKIE: &str = "inbox_session";
const TOKEN_REFRESH_SKEW_SECONDS: i64 = 300;
const MAX_CHANGED_THREADS_PER_SYNC: usize = 25;
const MAX_HISTORY_PAGES_PER_SYNC: usize = 5;
const SYNC_INTERVAL_SECONDS: u64 = 300;
const SCHEDULER_TICK_SECONDS: u64 = 30;
const MAX_ENQUEUES_PER_TICK: i64 = 10;
const MAX_GLOBAL_CONCURRENT_SYNCS: usize = 5;

#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
    config: AppConfig,
    http: reqwest::Client,
    sync_semaphore: Arc<Semaphore>,
}

#[derive(Clone)]
struct AppConfig {
    google_client_id: String,
    google_client_secret: String,
    app_base_url: String,
}

#[derive(Debug)]
struct AppError(String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        eprintln!("app_error: {}", self.0);
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, self.0).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        Self(format!("database error: {error}"))
    }
}

impl From<askama::Error> for AppError {
    fn from(error: askama::Error) -> Self {
        Self(format!("template render failed: {error}"))
    }
}

impl From<reqwest::Error> for AppError {
    fn from(error: reqwest::Error) -> Self {
        Self(format!("http error: {error}"))
    }
}

#[derive(Clone)]
struct UserSession {
    user_id: i64,
    email: String,
    openrouter_key: Option<String>,
}

struct SyncUser {
    user_id: i64,
    email: String,
    access_token: String,
    refresh_token: Option<String>,
    token_expires_at: Option<i64>,
    gmail_history_id: Option<String>,
    gmail_reauth_required: bool,
}

#[derive(FromRow)]
struct ThreadRow {
    id: i64,
    subject: String,
    from_display: String,
    state: String,
    latest_action: String,
    waiting_until: Option<String>,
}

struct ThreadView {
    id: i64,
    subject: String,
    from_display: String,
    state_label: &'static str,
    state_class: &'static str,
    latest_action: String,
    waiting_until: Option<String>,
}

impl From<ThreadRow> for ThreadView {
    fn from(row: ThreadRow) -> Self {
        Self {
            state_label: state_label(&row.state),
            state_class: state_class(&row.state),
            id: row.id,
            subject: row.subject,
            from_display: row.from_display,
            latest_action: row.latest_action,
            waiting_until: row.waiting_until,
        }
    }
}

#[derive(FromRow)]
struct MessageRow {
    sender: String,
    body: String,
    sent_at: String,
}

#[derive(FromRow)]
struct UserFormRow {
    form_type: String,
    title: String,
    body: String,
}

#[derive(FromRow)]
struct ActionLogRow {
    actor: String,
    message: String,
    created_at: String,
}

#[derive(FromRow)]
struct ChatRow {
    role: String,
    message: String,
}

struct FilterOption<'a> {
    value: &'a str,
    label: &'a str,
    selected: bool,
}

#[derive(serde::Deserialize)]
struct InboxQuery {
    state: Option<String>,
}

#[derive(serde::Deserialize)]
struct GoogleCallbackQuery {
    code: Option<String>,
    error: Option<String>,
}

#[derive(serde::Deserialize)]
struct UserSubmission {
    action: String,
    response: Option<String>,
    draft_body: Option<String>,
    comment: Option<String>,
}

#[derive(serde::Deserialize)]
struct SettingsSubmission {
    openrouter_key: String,
}

#[derive(serde::Deserialize)]
struct GoogleTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

#[derive(serde::Deserialize)]
struct GmailProfileResponse {
    #[serde(rename = "emailAddress")]
    email_address: String,
    #[serde(rename = "historyId")]
    history_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct GmailHistoryResponse {
    history: Option<Vec<GmailHistory>>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(rename = "historyId")]
    history_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct GmailHistory {
    #[serde(rename = "messagesAdded")]
    messages_added: Option<Vec<GmailHistoryMessage>>,
    #[serde(rename = "messagesDeleted")]
    messages_deleted: Option<Vec<GmailHistoryMessage>>,
    #[serde(rename = "labelsAdded")]
    labels_added: Option<Vec<GmailHistoryMessage>>,
    #[serde(rename = "labelsRemoved")]
    labels_removed: Option<Vec<GmailHistoryMessage>>,
}

#[derive(serde::Deserialize)]
struct GmailHistoryMessage {
    message: GmailHistoryMessageRef,
}

#[derive(serde::Deserialize)]
struct GmailHistoryMessageRef {
    #[serde(rename = "threadId")]
    thread_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct GmailThreadsResponse {
    threads: Option<Vec<GmailThreadStub>>,
}

#[derive(serde::Deserialize)]
struct GmailThreadStub {
    id: String,
}

#[derive(serde::Deserialize)]
struct GmailThreadResponse {
    id: String,
    snippet: Option<String>,
    messages: Vec<GmailMessage>,
}

#[derive(serde::Deserialize)]
struct GmailMessage {
    id: String,
    #[serde(rename = "internalDate")]
    internal_date: Option<String>,
    payload: GmailPayload,
}

#[derive(serde::Deserialize)]
struct GmailPayload {
    headers: Vec<GmailHeader>,
    body: Option<GmailBody>,
    parts: Option<Vec<Self>>,
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
}

#[derive(serde::Deserialize)]
struct GmailHeader {
    name: String,
    value: String,
}

#[derive(serde::Deserialize)]
struct GmailBody {
    data: Option<String>,
}

#[derive(Template)]
#[template(path = "page.html")]
struct PageTemplate<'a> {
    title: &'a str,
    heading: &'a str,
    body: &'a str,
    user_email: &'a str,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate;

#[derive(Template)]
#[template(path = "inbox.html")]
struct InboxTemplate<'a> {
    threads: Vec<ThreadView>,
    filters: Vec<FilterOption<'a>>,
    user_email: String,
    syncing: bool,
}

#[derive(Template)]
#[template(path = "inbox_table.html")]
struct InboxTableTemplate {
    threads: Vec<ThreadView>,
}

#[derive(Template)]
#[template(path = "sync_button.html")]
struct SyncButtonTemplate {
    syncing: bool,
}

#[derive(Template)]
#[template(path = "thread.html")]
struct ThreadTemplate {
    thread: ThreadView,
    messages: Vec<MessageRow>,
    form: Option<UserFormRow>,
    logs: Vec<ActionLogRow>,
    chats: Vec<ChatRow>,
    user_email: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    user_email: String,
    openrouter_key: String,
}

#[derive(serde::Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
enum ClassifierOutput {
    NeedsReply,
    Waiting { reason: String, wake_date: String },
    AskToResolve { reason: String },
}

#[derive(serde::Deserialize)]
struct DraftOutput {
    draft: String,
    message: Option<String>,
}

enum DraftInstruction<'a> {
    Initial,
    Revision {
        current_draft: &'a str,
        latest_comment: &'a str,
    },
}

fn render(template: impl Template) -> Result<Response, AppError> {
    let html = template.render()?;
    Ok(Html(html).into_response())
}

async fn style_css() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../static/style.css"),
    )
        .into_response()
}

async fn login() -> Result<Response, AppError> {
    render(LoginTemplate)
}

async fn google_start(State(state): State<AppState>) -> Result<Response, AppError> {
    eprintln!("oauth_start: redirecting to Google");
    let mut url = Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
        .map_err(|error| AppError(format!("invalid google auth URL: {error}")))?;
    url.query_pairs_mut()
        .append_pair("client_id", &state.config.google_client_id)
        .append_pair("redirect_uri", &redirect_uri(&state.config))
        .append_pair("response_type", "code")
        .append_pair(
            "scope",
            "https://www.googleapis.com/auth/gmail.readonly https://www.googleapis.com/auth/gmail.send",
        )
        .append_pair("access_type", "offline")
        .append_pair("include_granted_scopes", "true");

    Ok(Redirect::to(url.as_str()).into_response())
}

async fn google_callback(
    State(state): State<AppState>,
    Query(query): Query<GoogleCallbackQuery>,
) -> Result<Response, AppError> {
    if let Some(error) = query.error {
        return Ok((
            axum::http::StatusCode::BAD_REQUEST,
            format!("Google OAuth failed: {error}"),
        )
            .into_response());
    }

    let code = query
        .code
        .ok_or_else(|| AppError("missing OAuth code".to_owned()))?;
    eprintln!("oauth_callback: exchanging code");
    let token = exchange_google_code(&state, &code).await?;
    eprintln!("oauth_callback: fetching Gmail profile");
    let email = fetch_gmail_profile_email(&state, &token.access_token).await?;
    eprintln!("oauth_callback: upserting user {email}");
    let user_id = upsert_user(&state.pool, &email, &token).await?;
    sync_gmail_changes(&state, user_id).await?;
    let session_token = create_session(&state.pool, user_id).await?;

    let mut response = Redirect::to("/").into_response();
    let cookie = format!(
        "{SESSION_COOKIE}={session_token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000"
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie)
            .map_err(|error| AppError(format!("invalid session cookie: {error}")))?,
    );
    Ok(response)
}

async fn logout(headers: HeaderMap, State(state): State<AppState>) -> Result<Response, AppError> {
    if let Some(session_token) = session_token_from_headers(&headers) {
        sqlx::query("DELETE FROM sessions WHERE token = ?")
            .bind(session_token)
            .execute(&state.pool)
            .await?;
    }

    let mut response = Redirect::to("/login").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("inbox_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
    );
    Ok(response)
}

async fn inbox(
    headers: HeaderMap,
    State(state): State<AppState>,
    Query(query): Query<InboxQuery>,
) -> Result<Response, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let selected_filter = query.state.as_deref().unwrap_or("NeedsAction");
    let selected_state = Some(selected_filter).filter(|value| *value != "all");
    let threads = fetch_thread_views(&state.pool, session.user_id, selected_state).await?;

    render(InboxTemplate {
        threads,
        filters: filters(selected_filter),
        user_email: session.email,
        syncing: sync_job_is_running(&state.pool, session.user_id).await?,
    })
}

async fn inbox_table(
    headers: HeaderMap,
    State(state): State<AppState>,
    Query(query): Query<InboxQuery>,
) -> Result<Response, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let selected_filter = query.state.as_deref().unwrap_or("NeedsAction");
    let selected_state = Some(selected_filter).filter(|value| *value != "all");
    let threads = fetch_thread_views(&state.pool, session.user_id, selected_state).await?;

    render(InboxTableTemplate { threads })
}

async fn sync_button(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    render(SyncButtonTemplate {
        syncing: sync_job_is_running(&state.pool, session.user_id).await?,
    })
}

async fn settings(headers: HeaderMap, State(state): State<AppState>) -> Result<Response, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login").into_response());
    };

    render(SettingsTemplate {
        user_email: session.email,
        openrouter_key: session.openrouter_key.unwrap_or_default(),
    })
}

async fn save_settings(
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

async fn reset_synced_data(
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

async fn sync_gmail(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Redirect, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login"));
    };

    if active_sync_job_exists(&state.pool, session.user_id).await? {
        eprintln!(
            "sync_route: existing sync in progress user_id={} email={}",
            session.user_id, session.email
        );
        return Ok(Redirect::to("/"));
    }

    let job_id = create_sync_job(&state.pool, session.user_id).await?;
    eprintln!(
        "sync_route: queued job_id={job_id} for user_id={} email={}",
        session.user_id, session.email
    );
    let worker_state = state.clone();
    eprintln!("sync_route: spawning worker job_id={job_id}");
    tokio::spawn(async move {
        eprintln!("sync_job: spawned job_id={job_id}");
        if let Err(error) = run_sync_job(worker_state, session.user_id, session.email, job_id).await
        {
            eprintln!("sync_job: failed job_id={job_id} error={}", error.0);
        }
    });
    Ok(Redirect::to("/"))
}

async fn thread(
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

async fn submit_thread_form(
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
            handle_draft_revision(&state, &session, id, &submission).await?;
        }
        (Some("DraftReply"), "resolve") => {
            submit_active_forms(&state.pool, id).await?;
            set_thread_state(&state.pool, id, "Resolved", "User resolved thread", None).await?;
            log_thread_event(&state.pool, id, "system", "State changed to Resolved").await?;
            return Ok(Redirect::to("/"));
        }
        (Some("AskToResolve" | "AskUser"), "comment") => {
            handle_classifier_comment(&state, &session, id, &submission).await?;
        }
        _ => {
            submit_active_forms(&state.pool, id).await?;
            set_thread_state(&state.pool, id, "Processing", "Classifier running", None).await?;
            run_classifier(&state, &session, id).await?;
        }
    }

    Ok(Redirect::to(&format!("/threads/{id}")))
}

async fn handle_draft_revision(
    state: &AppState,
    session: &UserSession,
    thread_id: i64,
    submission: &UserSubmission,
) -> Result<(), AppError> {
    let comment = submission.comment.as_deref().unwrap_or_default().trim();
    if !comment.is_empty() {
        insert_thread_chat(&state.pool, thread_id, "user", comment).await?;
    }
    set_thread_state(
        &state.pool,
        thread_id,
        "Processing",
        "Revising draft reply",
        None,
    )
    .await?;
    if let Err(error) = run_reply_drafter(
        state,
        session,
        thread_id,
        DraftInstruction::Revision {
            current_draft: submission.draft_body.as_deref().unwrap_or_default(),
            latest_comment: comment,
        },
    )
    .await
    {
        set_thread_state(
            &state.pool,
            thread_id,
            "NeedsAction",
            "Draft revision failed",
            None,
        )
        .await?;
        log_thread_event(
            &state.pool,
            thread_id,
            "system",
            &format!("Draft revision failed: {}", error.0),
        )
        .await?;
        return Err(error);
    }
    Ok(())
}

async fn handle_classifier_comment(
    state: &AppState,
    session: &UserSession,
    thread_id: i64,
    submission: &UserSubmission,
) -> Result<(), AppError> {
    let comment = submission.comment.as_deref().unwrap_or_default().trim();
    if !comment.is_empty() {
        insert_thread_chat(&state.pool, thread_id, "user", comment).await?;
    }
    submit_active_forms(&state.pool, thread_id).await?;
    set_thread_state(
        &state.pool,
        thread_id,
        "Processing",
        "Classifier running",
        None,
    )
    .await?;
    if let Err(error) = run_classifier(state, session, thread_id).await {
        set_thread_state(
            &state.pool,
            thread_id,
            "NeedsAction",
            "Classifier failed",
            None,
        )
        .await?;
        log_thread_event(
            &state.pool,
            thread_id,
            "system",
            &format!("Classifier failed after user comment: {}", error.0),
        )
        .await?;
        return Err(error);
    }
    Ok(())
}

async fn run_sync_job(
    state: AppState,
    user_id: i64,
    email: String,
    job_id: i64,
) -> Result<(), AppError> {
    let _permit = state
        .sync_semaphore
        .acquire()
        .await
        .map_err(|error| AppError(format!("sync semaphore closed: {error}")))?;
    update_sync_job(&state.pool, job_id, "running", "Starting Gmail sync", false).await?;
    eprintln!("sync_job: running job_id={job_id} user_id={user_id} email={email}");
    match sync_gmail_changes(&state, user_id).await {
        Ok(()) => {
            update_sync_job(&state.pool, job_id, "done", "Gmail sync complete", true).await?;
            eprintln!("sync_job: done job_id={job_id}");
            Ok(())
        }
        Err(error) => {
            update_sync_job(&state.pool, job_id, "failed", &error.0, true).await?;
            Err(error)
        }
    }
}

async fn create_sync_job(pool: &SqlitePool, user_id: i64) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO sync_jobs (user_id, status, message) VALUES (?, 'queued', 'Sync queued')",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    let job_id = result.last_insert_rowid();
    eprintln!("sync_job: created job_id={job_id} user_id={user_id} status=queued");
    Ok(job_id)
}

async fn update_sync_job(
    pool: &SqlitePool,
    job_id: i64,
    status: &str,
    message: &str,
    completed: bool,
) -> Result<(), sqlx::Error> {
    eprintln!("sync_job: update job_id={job_id} status={status} message={message}");
    if completed {
        sqlx::query(
            "UPDATE sync_jobs SET status = ?, message = ?, completed_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(status)
        .bind(message)
        .bind(job_id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "UPDATE sync_jobs SET status = ?, message = ?, started_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(status)
        .bind(message)
        .bind(job_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn sync_job_is_running(pool: &SqlitePool, user_id: i64) -> Result<bool, sqlx::Error> {
    let status: Option<(String,)> = sqlx::query_as(
        "SELECT status FROM sync_jobs
         WHERE user_id = ?
           AND (
             (status = 'queued' AND created_at > datetime('now', '-30 seconds'))
             OR (status = 'running' AND started_at > datetime('now', '-10 minutes'))
           )
         ORDER BY id DESC LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(status.is_some())
}

async fn active_sync_job_exists(pool: &SqlitePool, user_id: i64) -> Result<bool, sqlx::Error> {
    let status: Option<(String,)> = sqlx::query_as(
        "SELECT status FROM sync_jobs
         WHERE user_id = ? AND status IN ('queued', 'running')
         ORDER BY id DESC LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(status.is_some())
}

async fn scheduler_loop(state: AppState) {
    eprintln!("sync_scheduler: loop started");
    loop {
        if let Err(error) = enqueue_due_sync_jobs(&state).await {
            eprintln!("sync_scheduler: error={}", error.0);
        }
        tokio::time::sleep(std::time::Duration::from_secs(SCHEDULER_TICK_SECONDS)).await;
    }
}

async fn enqueue_due_sync_jobs(state: &AppState) -> Result<(), AppError> {
    let due_users = due_sync_users(&state.pool).await?;
    for (user_id, email) in due_users {
        let job_id = create_sync_job(&state.pool, user_id).await?;
        eprintln!("sync_scheduler: queued job_id={job_id} user_id={user_id} email={email}");
        let worker_state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = run_sync_job(worker_state, user_id, email, job_id).await {
                eprintln!("sync_scheduler: failed job_id={job_id} error={}", error.0);
            }
        });
    }
    Ok(())
}

async fn due_sync_users(pool: &SqlitePool) -> Result<Vec<(i64, String)>, sqlx::Error> {
    let interval = format!("-{SYNC_INTERVAL_SECONDS} seconds");
    sqlx::query_as(
        "SELECT users.id, users.google_email
         FROM users
         LEFT JOIN (
           SELECT user_id, MAX(completed_at) AS last_completed_at
           FROM sync_jobs
           WHERE completed_at IS NOT NULL
           GROUP BY user_id
         ) latest_sync ON latest_sync.user_id = users.id
         WHERE users.gmail_reauth_required = 0
           AND users.refresh_token IS NOT NULL
           AND NOT EXISTS (
             SELECT 1 FROM sync_jobs active
             WHERE active.user_id = users.id
               AND active.status IN ('queued', 'running')
           )
           AND (
             latest_sync.last_completed_at IS NULL
             OR latest_sync.last_completed_at <= datetime('now', ?)
           )
         ORDER BY COALESCE(latest_sync.last_completed_at, '1970-01-01'), random()
         LIMIT ?",
    )
    .bind(interval)
    .bind(MAX_ENQUEUES_PER_TICK)
    .fetch_all(pool)
    .await
}

async fn fetch_thread_views(
    pool: &SqlitePool,
    user_id: i64,
    selected_state: Option<&str>,
) -> Result<Vec<ThreadView>, sqlx::Error> {
    let thread_rows = if let Some(selected_state) = selected_state {
        sqlx::query_as::<_, ThreadRow>(
            "SELECT id, subject, from_display, state, latest_action, waiting_until FROM threads WHERE user_id = ? AND state = ? ORDER BY updated_at DESC",
        )
        .bind(user_id)
        .bind(selected_state)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, ThreadRow>(
            "SELECT id, subject, from_display, state, latest_action, waiting_until FROM threads WHERE user_id = ? ORDER BY updated_at DESC",
        )
        .bind(user_id)
        .fetch_all(pool)
        .await?
    };
    Ok(thread_rows.into_iter().map(ThreadView::from).collect())
}

fn filters(selected: &str) -> Vec<FilterOption<'_>> {
    [
        ("NeedsAction", "Needs Action"),
        ("Unprocessed", "Unprocessed"),
        ("all", "All"),
        ("Waiting", "Waiting"),
        ("Processing", "Processing"),
        ("Resolved", "Resolved"),
    ]
    .into_iter()
    .map(|(value, label)| FilterOption {
        value,
        label,
        selected: value == selected,
    })
    .collect()
}

fn state_label(state: &str) -> &'static str {
    match state {
        "NeedsAction" => "Needs Action",
        "Unprocessed" => "Unprocessed",
        "Processing" => "Processing",
        "Waiting" => "Waiting",
        "Resolved" => "Resolved",
        _ => "Unknown",
    }
}

fn state_class(state: &str) -> &'static str {
    match state {
        "NeedsAction" => "needs-action",
        "Unprocessed" => "unprocessed",
        "Processing" => "processing",
        "Waiting" => "waiting",
        "Resolved" => "resolved",
        _ => "unknown",
    }
}

async fn sync_gmail_changes(state: &AppState, user_id: i64) -> Result<(), AppError> {
    let mut sync_user = load_sync_user(&state.pool, user_id).await?;
    if sync_user.gmail_reauth_required {
        return Err(AppError("Reconnect Gmail before syncing".to_owned()));
    }
    let access_token = ensure_gmail_token(state, &mut sync_user).await?;
    let result = if sync_user.gmail_history_id.is_some() {
        sync_incremental_history(state, &sync_user, &access_token).await
    } else {
        bootstrap_latest_threads(state, &sync_user, &access_token).await
    };
    if let Err(error) = result {
        if !is_gmail_unauthorized(&error) {
            return Err(error);
        }
        eprintln!("gmail_sync: got 401; refreshing token and retrying once");
        let access_token = refresh_gmail_token(state, &mut sync_user).await?;
        if sync_user.gmail_history_id.is_some() {
            sync_incremental_history(state, &sync_user, &access_token).await?;
        } else {
            bootstrap_latest_threads(state, &sync_user, &access_token).await?;
        }
    }
    log_thread_state_counts(&state.pool, user_id, "gmail_sync_complete").await?;
    Ok(())
}

async fn bootstrap_latest_threads(
    state: &AppState,
    sync_user: &SyncUser,
    access_token: &str,
) -> Result<(), AppError> {
    eprintln!(
        "gmail_sync: bootstrap latest 10 user_id={} email={}",
        sync_user.user_id, sync_user.email
    );
    let profile = fetch_gmail_profile(state, access_token).await?;
    let history_id = profile
        .history_id
        .ok_or_else(|| AppError("Gmail profile did not include historyId".to_owned()))?;
    let response = state
        .http
        .get("https://gmail.googleapis.com/gmail/v1/users/me/threads")
        .bearer_auth(access_token)
        .query(&[("maxResults", "10")])
        .send()
        .await?;
    let status = response.status();
    eprintln!("gmail_sync: threads.list status={status}");
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
        return Err(AppError(format!(
            "Gmail threads.list failed status={status} body={}",
            preview(&body)
        )));
    }
    let threads = response.json::<GmailThreadsResponse>().await?;
    let threads = threads.threads.unwrap_or_default();
    eprintln!(
        "gmail_sync: received {} bootstrap thread stubs",
        threads.len()
    );
    fetch_and_upsert_threads(state, sync_user.user_id, access_token, &threads).await?;
    sqlx::query(
        "UPDATE users SET gmail_history_id = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(history_id)
    .bind(sync_user.user_id)
    .execute(&state.pool)
    .await?;
    Ok(())
}

async fn sync_incremental_history(
    state: &AppState,
    sync_user: &SyncUser,
    access_token: &str,
) -> Result<(), AppError> {
    let start_history_id = sync_user
        .gmail_history_id
        .as_deref()
        .ok_or_else(|| AppError("missing Gmail history cursor".to_owned()))?;
    eprintln!(
        "gmail_sync: incremental user_id={} start_history_id={start_history_id}",
        sync_user.user_id
    );
    let mut page_token: Option<String> = None;
    let mut changed_thread_ids = HashSet::new();
    let mut latest_history_id: Option<String> = None;
    let mut complete = true;

    for page_index in 0..MAX_HISTORY_PAGES_PER_SYNC {
        let mut request = state
            .http
            .get("https://gmail.googleapis.com/gmail/v1/users/me/history")
            .bearer_auth(access_token)
            .query(&[("startHistoryId", start_history_id)]);
        if let Some(token) = page_token.as_deref() {
            request = request.query(&[("pageToken", token)]);
        }
        let response = request.send().await?;
        let status = response.status();
        eprintln!(
            "gmail_sync: history.list page={} status={status}",
            page_index + 1
        );
        if status == axum::http::StatusCode::NOT_FOUND {
            eprintln!("gmail_sync: history cursor invalid; clearing and bootstrapping latest 10");
            sqlx::query(
                "UPDATE users SET gmail_history_id = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            )
            .bind(sync_user.user_id)
            .execute(&state.pool)
            .await?;
            bootstrap_latest_threads(state, sync_user, access_token).await?;
            return Ok(());
        }
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
            return Err(AppError(format!(
                "Gmail history.list failed status={status} body={}",
                preview(&body)
            )));
        }
        let history = response.json::<GmailHistoryResponse>().await?;
        latest_history_id = history.history_id.or(latest_history_id);
        collect_changed_thread_ids(
            &history.history.unwrap_or_default(),
            &mut changed_thread_ids,
        );
        page_token = history.next_page_token;
        if changed_thread_ids.len() > MAX_CHANGED_THREADS_PER_SYNC {
            complete = false;
            break;
        }
        if page_token.is_none() {
            break;
        }
        if page_index + 1 == MAX_HISTORY_PAGES_PER_SYNC {
            complete = false;
        }
    }

    let mut thread_ids = changed_thread_ids.into_iter().collect::<Vec<_>>();
    thread_ids.sort();
    if thread_ids.len() > MAX_CHANGED_THREADS_PER_SYNC {
        thread_ids.truncate(MAX_CHANGED_THREADS_PER_SYNC);
        complete = false;
    }

    let stubs = thread_ids
        .into_iter()
        .map(|id| GmailThreadStub { id })
        .collect::<Vec<_>>();
    fetch_and_upsert_threads(state, sync_user.user_id, access_token, &stubs).await?;

    if complete {
        if let Some(history_id) = latest_history_id {
            sqlx::query(
                "UPDATE users SET gmail_history_id = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            )
            .bind(history_id)
            .bind(sync_user.user_id)
            .execute(&state.pool)
            .await?;
        }
    } else {
        eprintln!(
            "gmail_sync: incremental cap hit user_id={}; leaving history cursor unchanged",
            sync_user.user_id
        );
    }
    Ok(())
}

fn is_gmail_unauthorized(error: &AppError) -> bool {
    error.0.contains("status=401") || error.0.contains("401 Unauthorized")
}

fn collect_changed_thread_ids(history: &[GmailHistory], changed_thread_ids: &mut HashSet<String>) {
    for entry in history {
        collect_history_messages(entry.messages_added.as_deref(), changed_thread_ids);
        collect_history_messages(entry.messages_deleted.as_deref(), changed_thread_ids);
        collect_history_messages(entry.labels_added.as_deref(), changed_thread_ids);
        collect_history_messages(entry.labels_removed.as_deref(), changed_thread_ids);
    }
}

fn collect_history_messages(
    messages: Option<&[GmailHistoryMessage]>,
    changed_thread_ids: &mut HashSet<String>,
) {
    if let Some(messages) = messages {
        for message in messages {
            if let Some(thread_id) = message.message.thread_id.as_deref() {
                changed_thread_ids.insert(thread_id.to_owned());
            }
        }
    }
}

async fn fetch_and_upsert_threads(
    state: &AppState,
    user_id: i64,
    access_token: &str,
    threads: &[GmailThreadStub],
) -> Result<(), AppError> {
    for (index, thread) in threads.iter().enumerate() {
        eprintln!(
            "gmail_sync: fetching thread {}/{} gmail_thread_id={}",
            index + 1,
            threads.len(),
            thread.id
        );
        let Some(gmail_thread) = fetch_gmail_thread(state, access_token, &thread.id).await? else {
            eprintln!(
                "gmail_sync: thread missing/removed gmail_thread_id={}",
                thread.id
            );
            continue;
        };
        eprintln!(
            "gmail_sync: fetched gmail_thread_id={} messages={}",
            gmail_thread.id,
            gmail_thread.messages.len()
        );
        let thread_id = upsert_gmail_thread(&state.pool, user_id, &gmail_thread).await?;
        log_thread_event(
            &state.pool,
            thread_id,
            "system",
            "Queued processing after Gmail sync",
        )
        .await?;
        let (state_name, latest_action): (String, String) =
            sqlx::query_as("SELECT state, latest_action FROM threads WHERE id = ?")
                .bind(thread_id)
                .fetch_one(&state.pool)
                .await?;
        eprintln!(
            "gmail_sync: upserted local thread_id={thread_id}; state={state_name} latest_action={latest_action}"
        );
    }
    Ok(())
}

async fn fetch_gmail_thread(
    state: &AppState,
    access_token: &str,
    gmail_thread_id: &str,
) -> Result<Option<GmailThreadResponse>, AppError> {
    eprintln!("gmail_thread_get: request gmail_thread_id={gmail_thread_id}");
    let response = state
        .http
        .get(format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/threads/{gmail_thread_id}"
        ))
        .bearer_auth(access_token)
        .query(&[("format", "full")])
        .send()
        .await?;
    let status = response.status();
    eprintln!("gmail_thread_get: response gmail_thread_id={gmail_thread_id} status={status}");
    if status == axum::http::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
        return Err(AppError(format!(
            "Gmail thread get failed gmail_thread_id={gmail_thread_id} status={status} body={}",
            preview(&body)
        )));
    }
    Ok(Some(response.json::<GmailThreadResponse>().await?))
}

async fn upsert_gmail_thread(
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

async fn processor_loop(state: AppState) {
    eprintln!("processor: loop started");
    loop {
        match process_next_unprocessed_thread(&state).await {
            Ok(true) => {}
            Ok(false) => tokio::time::sleep(std::time::Duration::from_secs(3)).await,
            Err(error) => {
                eprintln!("processor: loop error={}", error.0);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
}

async fn process_next_unprocessed_thread(state: &AppState) -> Result<bool, AppError> {
    let Some(thread_id) = next_unprocessed_thread_id(&state.pool).await? else {
        return Ok(false);
    };
    if !claim_unprocessed_thread(&state.pool, thread_id).await? {
        return Ok(true);
    }
    eprintln!("processor: claimed thread_id={thread_id}");
    let session = user_session_for_thread(&state.pool, thread_id).await?;
    log_thread_event(&state.pool, thread_id, "system", "Processing started").await?;
    if let Err(error) = run_classifier(state, &session, thread_id).await {
        set_thread_state(
            &state.pool,
            thread_id,
            "NeedsAction",
            "Processing failed",
            None,
        )
        .await?;
        log_thread_event(
            &state.pool,
            thread_id,
            "system",
            &format!("Processing failed: {}", error.0),
        )
        .await?;
        return Err(error);
    }
    let (state_name, latest_action): (String, String) =
        sqlx::query_as("SELECT state, latest_action FROM threads WHERE id = ?")
            .bind(thread_id)
            .fetch_one(&state.pool)
            .await?;
    eprintln!(
        "processor: completed thread_id={thread_id} state={state_name} latest_action={latest_action}"
    );
    log_thread_state_counts(&state.pool, session.user_id, "processor_complete").await?;
    Ok(true)
}

async fn next_unprocessed_thread_id(pool: &SqlitePool) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM threads WHERE state = 'Unprocessed' ORDER BY updated_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}

async fn user_session_for_thread(
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

async fn claim_unprocessed_thread(pool: &SqlitePool, thread_id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE threads SET state = 'Processing', latest_action = 'Classifier running', waiting_until = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND state = 'Unprocessed'",
    )
    .bind(thread_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

async fn run_classifier(
    state: &AppState,
    session: &UserSession,
    thread_id: i64,
) -> Result<(), AppError> {
    eprintln!("classifier: start thread_id={thread_id}");
    let Some(openrouter_key) = session.openrouter_key.as_deref() else {
        eprintln!("classifier: missing OpenRouter key thread_id={thread_id}");
        create_user_form(
            &state.pool,
            thread_id,
            "AskUser",
            "OpenRouter key required",
            "Add your OpenRouter API key in Settings, then sync again.",
        )
        .await?;
        set_thread_state(
            &state.pool,
            thread_id,
            "NeedsAction",
            "OpenRouter key required",
            None,
        )
        .await?;
        return Ok(());
    };

    log_thread_event(&state.pool, thread_id, "agent", "Classifier started").await?;
    let context = thread_context(&state.pool, thread_id).await?;
    eprintln!(
        "classifier: built context thread_id={thread_id} bytes={}",
        context.len()
    );
    let prompt = format!(
        "Classify this email thread. Return only JSON matching one of: {{\"decision\":\"needs_reply\"}}, {{\"decision\":\"waiting\",\"reason\":\"...\",\"wake_date\":\"YYYY-MM-DD\"}}, {{\"decision\":\"ask_to_resolve\",\"reason\":\"...\"}}. Use needs_reply only when the user owes a reply. Use waiting only when the user has already acted and we need an external response; waiting must include a wake_date. Use ask_to_resolve when no reply is needed and no external response is expected. The classifier cannot directly resolve threads.\n\n{context}"
    );
    eprintln!(
        "classifier: calling OpenRouter thread_id={thread_id} prompt_bytes={}",
        prompt.len()
    );
    let output = call_openrouter_json::<ClassifierOutput>(
        &state.http,
        openrouter_key,
        &prompt,
        "Return exactly one JSON object: {\"decision\":\"needs_reply\"} OR {\"decision\":\"waiting\",\"reason\":\"...\",\"wake_date\":\"YYYY-MM-DD\"} OR {\"decision\":\"ask_to_resolve\",\"reason\":\"...\"}.",
    )
    .await?;

    match output {
        ClassifierOutput::NeedsReply => {
            eprintln!("classifier: decision=needs_reply thread_id={thread_id}");
            set_thread_state(&state.pool, thread_id, "Processing", "Drafting reply", None).await?;
            run_reply_drafter(state, session, thread_id, DraftInstruction::Initial).await?;
        }
        ClassifierOutput::Waiting { reason, wake_date } => {
            eprintln!(
                "classifier: decision=waiting thread_id={thread_id} wake_date={wake_date} reason={reason}"
            );
            set_thread_state(
                &state.pool,
                thread_id,
                "Waiting",
                &format!("Waiting: {reason}"),
                Some(&wake_date),
            )
            .await?;
            log_thread_event(
                &state.pool,
                thread_id,
                "agent",
                &format!("Classifier set Waiting until {wake_date}: {reason}"),
            )
            .await?;
        }
        ClassifierOutput::AskToResolve { reason } => {
            eprintln!("classifier: decision=ask_to_resolve thread_id={thread_id} reason={reason}");
            create_user_form(
                &state.pool,
                thread_id,
                "AskToResolve",
                "Can this be resolved?",
                &reason,
            )
            .await?;
            set_thread_state(
                &state.pool,
                thread_id,
                "NeedsAction",
                "Confirm resolved",
                None,
            )
            .await?;
            log_thread_event(
                &state.pool,
                thread_id,
                "agent",
                "Classifier asked user to resolve",
            )
            .await?;
        }
    }

    eprintln!("classifier: complete thread_id={thread_id}");
    Ok(())
}

async fn run_reply_drafter(
    state: &AppState,
    session: &UserSession,
    thread_id: i64,
    instruction: DraftInstruction<'_>,
) -> Result<(), AppError> {
    eprintln!("reply_drafter: start thread_id={thread_id}");
    let Some(openrouter_key) = session.openrouter_key.as_deref() else {
        eprintln!("reply_drafter: missing OpenRouter key thread_id={thread_id}");
        return Err(AppError("OpenRouter key required".to_owned()));
    };

    log_thread_event(&state.pool, thread_id, "agent", "Reply drafter started").await?;
    let context = thread_context(&state.pool, thread_id).await?;
    eprintln!(
        "reply_drafter: built context thread_id={thread_id} bytes={}",
        context.len()
    );
    let chats = thread_chats(&state.pool, thread_id).await?;
    let prompt = draft_prompt(&context, &chats, &instruction);
    eprintln!(
        "reply_drafter: calling OpenRouter thread_id={thread_id} prompt_bytes={}",
        prompt.len()
    );
    let output = call_openrouter_json::<DraftOutput>(
        &state.http,
        openrouter_key,
        &prompt,
        "Return exactly one JSON object: {\"draft\":\"...\",\"message\":\"short note to user\"}.",
    )
    .await?;
    eprintln!(
        "reply_drafter: parsed draft thread_id={thread_id} bytes={}",
        output.draft.len()
    );

    create_user_form(
        &state.pool,
        thread_id,
        "DraftReply",
        "Review draft reply",
        &output.draft,
    )
    .await?;
    set_thread_state(
        &state.pool,
        thread_id,
        "NeedsAction",
        "Review draft reply",
        None,
    )
    .await?;
    let chat_message = output.message.as_deref().unwrap_or(match instruction {
        DraftInstruction::Initial => "I drafted a reply for review.",
        DraftInstruction::Revision { .. } => "Updated the draft.",
    });
    insert_thread_chat(&state.pool, thread_id, "assistant", chat_message).await?;
    log_thread_event(
        &state.pool,
        thread_id,
        "agent",
        "Reply drafter created draft form",
    )
    .await?;
    eprintln!("reply_drafter: complete thread_id={thread_id}");
    Ok(())
}

async fn call_openrouter_json<T>(
    http: &reqwest::Client,
    api_key: &str,
    prompt: &str,
    schema_hint: &str,
) -> Result<T, AppError>
where
    T: serde::de::DeserializeOwned,
{
    let mut current_prompt = prompt.to_owned();
    let mut last_error = String::new();

    for attempt in 1..=3 {
        eprintln!("agent_json: attempt={attempt}");
        let model_response = call_openrouter(http, api_key, &current_prompt).await?;
        eprintln!(
            "agent_json: response attempt={attempt} bytes={} preview={}",
            model_response.len(),
            preview(&model_response)
        );
        match parse_json_content::<T>(&model_response) {
            Ok(output) => return Ok(output),
            Err(error) => {
                last_error = error.0;
                eprintln!("agent_json: parse_failed attempt={attempt} error={last_error}");
                current_prompt = format!(
                    "{prompt}\n\nYour previous response was invalid. {schema_hint}\nParse error: {last_error}\nPrevious response: {model_response}\nReturn corrected JSON only."
                );
            }
        }
    }

    Err(AppError(format!(
        "agent returned invalid JSON after retries: {last_error}"
    )))
}

async fn call_openrouter(
    http: &reqwest::Client,
    api_key: &str,
    prompt: &str,
) -> Result<String, AppError> {
    let models = [
        "inclusionai/ling-3.0-flash:free",
        "google/gemma-4-31b-it:free",
    ];
    let mut last_error = String::new();

    for model in models {
        eprintln!(
            "openrouter: request model={model} prompt_bytes={}",
            prompt.len()
        );
        let response = http
            .post("https://openrouter.ai/api/v1/chat/completions")
            .bearer_auth(api_key)
            .json(&serde_json::json!({
                "model": model,
                "messages": [{ "role": "user", "content": prompt }]
            }))
            .send()
            .await?;
        let status = response.status();
        eprintln!("openrouter: response model={model} status={status}");
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
            last_error = format!(
                "OpenRouter request failed model={model} status={status} body={}",
                preview(&body)
            );
            eprintln!("openrouter: {last_error}");
            continue;
        }

        let value = response.json::<serde_json::Value>().await?;
        eprintln!("openrouter: response JSON parsed model={model}");
        if let Some(content) = value["choices"][0]["message"]["content"].as_str() {
            return Ok(content.to_owned());
        }

        last_error = format!(
            "OpenRouter response missing content model={model} body={}",
            preview(&value.to_string())
        );
        eprintln!("openrouter: {last_error}");
    }

    Err(AppError(last_error))
}

fn preview(text: &str) -> String {
    const MAX_CHARS: usize = 300;
    let mut output: String = text.chars().take(MAX_CHARS).collect();
    if text.chars().count() > MAX_CHARS {
        output.push_str("...");
    }
    output.replace('\n', " ")
}

fn parse_json_content<T: serde::de::DeserializeOwned>(content: &str) -> Result<T, AppError> {
    let trimmed = content.trim();
    let json_text = trimmed
        .strip_prefix("```json")
        .and_then(|text| text.strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();
    serde_json::from_str(json_text)
        .map_err(|error| AppError(format!("agent returned invalid JSON: {error}: {content}")))
}

async fn thread_context(pool: &SqlitePool, thread_id: i64) -> Result<String, sqlx::Error> {
    let thread = sqlx::query_as::<_, ThreadRow>(
        "SELECT id, subject, from_display, state, latest_action, waiting_until FROM threads WHERE id = ?",
    )
    .bind(thread_id)
    .fetch_one(pool)
    .await?;
    let messages = sqlx::query_as::<_, MessageRow>(
        "SELECT sender, body, sent_at FROM messages WHERE thread_id = ? ORDER BY sent_at ASC",
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await?;
    let mut context = format!(
        "Subject: {}\nFrom: {}\n",
        thread.subject, thread.from_display
    );
    for message in messages {
        write!(
            context,
            "\nMessage from {} at {}:\n{}\n",
            message.sender, message.sent_at, message.body
        )
        .expect("writing to string cannot fail");
    }
    let chats = thread_chats(pool, thread_id).await?;
    if !chats.is_empty() {
        context.push_str("\nDraft assistant chat:\n");
        for chat in chats {
            let role = match chat.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                _ => "Other",
            };
            writeln!(context, "{role}: {}", chat.message).expect("writing to string cannot fail");
        }
    }
    Ok(context)
}

fn draft_prompt(context: &str, chats: &[ChatRow], instruction: &DraftInstruction<'_>) -> String {
    match instruction {
        DraftInstruction::Initial => format!(
            "Draft an email reply for this thread. Return only JSON: {{\"draft\":\"...\",\"message\":\"short note to user\"}}. Message should briefly explain what you drafted.\n\nEmail thread:\n{context}"
        ),
        DraftInstruction::Revision {
            current_draft,
            latest_comment,
        } => {
            let mut prompt = format!(
                "Revise the current draft using the latest user instruction. Return only JSON: {{\"draft\":\"...\",\"message\":\"short note to user\"}}. Message should briefly explain what changed.\n\nEmail thread:\n{context}\n\nCurrent draft:\n{current_draft}\n\nChat history:"
            );
            for chat in chats {
                let role = match chat.role.as_str() {
                    "user" => "User",
                    "assistant" => "Assistant",
                    _ => "Other",
                };
                write!(prompt, "\n{role}: {}", chat.message)
                    .expect("writing to string cannot fail");
            }
            write!(prompt, "\n\nLatest user instruction:\n{latest_comment}")
                .expect("writing to string cannot fail");
            prompt
        }
    }
}

async fn thread_chats(pool: &SqlitePool, thread_id: i64) -> Result<Vec<ChatRow>, sqlx::Error> {
    sqlx::query_as::<_, ChatRow>(
        "SELECT role, message FROM thread_chats WHERE thread_id = ? ORDER BY id ASC",
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await
}

async fn insert_thread_chat(
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

async fn submit_active_forms(pool: &SqlitePool, thread_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE user_forms SET status = 'submitted' WHERE thread_id = ? AND status = 'active'",
    )
    .bind(thread_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn create_user_form(
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

async fn set_thread_state(
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

async fn log_thread_event(
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

async fn log_thread_state_counts(
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

fn header_value<'a>(headers: &'a [GmailHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}

fn extract_body_text(payload: &GmailPayload) -> Option<String> {
    let mime_type = payload.mime_type.as_deref();
    if matches!(mime_type, Some("text/plain" | "text/html"))
        && let Some(data) = payload.body.as_ref().and_then(|body| body.data.as_ref())
    {
        return decode_gmail_body(data);
    }

    payload.parts.as_ref().and_then(|parts| {
        parts
            .iter()
            .find_map(|part| extract_body_text(part).filter(|body| !body.is_empty()))
    })
}

fn decode_gmail_body(data: &str) -> Option<String> {
    let padded = match data.len() % 4 {
        0 => data.to_owned(),
        remainder => format!("{}{}", data, "=".repeat(4 - remainder)),
    };
    URL_SAFE
        .decode(padded)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

async fn exchange_google_code(
    state: &AppState,
    code: &str,
) -> Result<GoogleTokenResponse, AppError> {
    let redirect_uri = redirect_uri(&state.config);
    let params = [
        ("code", code),
        ("client_id", state.config.google_client_id.as_str()),
        ("client_secret", state.config.google_client_secret.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("grant_type", "authorization_code"),
    ];
    let response = state
        .http
        .post("https://oauth2.googleapis.com/token")
        .form(&params)
        .send()
        .await?
        .error_for_status()?;
    Ok(response.json::<GoogleTokenResponse>().await?)
}

async fn fetch_gmail_profile(
    state: &AppState,
    access_token: &str,
) -> Result<GmailProfileResponse, AppError> {
    let response = state
        .http
        .get("https://gmail.googleapis.com/gmail/v1/users/me/profile")
        .bearer_auth(access_token)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
        return Err(AppError(format!(
            "Gmail profile failed status={status} body={}",
            preview(&body)
        )));
    }
    Ok(response.json::<GmailProfileResponse>().await?)
}

async fn fetch_gmail_profile_email(
    state: &AppState,
    access_token: &str,
) -> Result<String, AppError> {
    Ok(fetch_gmail_profile(state, access_token)
        .await?
        .email_address)
}

async fn upsert_user(
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

async fn load_sync_user(pool: &SqlitePool, user_id: i64) -> Result<SyncUser, sqlx::Error> {
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

async fn ensure_gmail_token(
    state: &AppState,
    sync_user: &mut SyncUser,
) -> Result<String, AppError> {
    if let Some(expires_at) = sync_user.token_expires_at
        && expires_at > unix_now() + TOKEN_REFRESH_SKEW_SECONDS
    {
        return Ok(sync_user.access_token.clone());
    }
    refresh_gmail_token(state, sync_user).await
}

async fn refresh_gmail_token(
    state: &AppState,
    sync_user: &mut SyncUser,
) -> Result<String, AppError> {
    let Some(refresh_token) = sync_user.refresh_token.as_deref() else {
        mark_gmail_reauth_required(&state.pool, sync_user.user_id).await?;
        return Err(AppError("Reconnect Gmail before syncing".to_owned()));
    };
    let params = [
        ("client_id", state.config.google_client_id.as_str()),
        ("client_secret", state.config.google_client_secret.as_str()),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ];
    let response = state
        .http
        .post("https://oauth2.googleapis.com/token")
        .form(&params)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
        mark_gmail_reauth_required(&state.pool, sync_user.user_id).await?;
        return Err(AppError(format!(
            "Google token refresh failed status={status} body={}",
            preview(&body)
        )));
    }
    let token = response.json::<GoogleTokenResponse>().await?;
    let expires_at = token.expires_in.map(|seconds| unix_now() + seconds);
    sqlx::query(
        "UPDATE users SET access_token = ?, expires_in = ?, token_expires_at = ?, gmail_reauth_required = 0, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(&token.access_token)
    .bind(token.expires_in)
    .bind(expires_at)
    .bind(sync_user.user_id)
    .execute(&state.pool)
    .await?;
    sync_user.access_token = token.access_token;
    sync_user.token_expires_at = expires_at;
    sync_user.gmail_reauth_required = false;
    Ok(sync_user.access_token.clone())
}

async fn mark_gmail_reauth_required(pool: &SqlitePool, user_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE users SET gmail_reauth_required = 1, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

async fn create_session(pool: &SqlitePool, user_id: i64) -> Result<String, sqlx::Error> {
    let token = random_token(pool).await?;
    sqlx::query("INSERT INTO sessions (token, user_id) VALUES (?, ?)")
        .bind(&token)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(token)
}

async fn require_user_session(
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

fn session_token_from_headers(headers: &HeaderMap) -> Option<&str> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        let trimmed = part.trim();
        trimmed
            .strip_prefix(SESSION_COOKIE)
            .and_then(|value| value.strip_prefix('='))
    })
}

async fn random_token(pool: &SqlitePool) -> Result<String, sqlx::Error> {
    let (token,): (String,) = sqlx::query_as("SELECT lower(hex(randomblob(32)))")
        .fetch_one(pool)
        .await?;
    Ok(token)
}

fn redirect_uri(config: &AppConfig) -> String {
    format!("{}/login/google/callback", config.app_base_url)
}

fn load_config() -> Result<AppConfig, Box<dyn Error>> {
    dotenvy::dotenv().ok();
    Ok(AppConfig {
        google_client_id: env::var("GOOGLE_CLIENT_ID")?,
        google_client_secret: env::var("GOOGLE_CLIENT_SECRET")?,
        app_base_url: env::var("APP_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:3000".to_owned()),
    })
}

async fn initialize_database() -> Result<SqlitePool, Box<dyn Error>> {
    let options = SqliteConnectOptions::from_str("sqlite:inbox.db")?.create_if_missing(true);
    let pool = SqlitePool::connect_with(options).await?;
    sqlx::migrate!().run(&pool).await?;
    fail_interrupted_sync_jobs(&pool).await?;
    Ok(pool)
}

async fn fail_interrupted_sync_jobs(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let result = sqlx::query(
        "UPDATE sync_jobs
         SET status = 'failed', message = 'Sync interrupted by server restart', completed_at = CURRENT_TIMESTAMP
         WHERE status IN ('queued', 'running')",
    )
    .execute(pool)
    .await?;
    if result.rows_affected() > 0 {
        eprintln!(
            "sync_job: marked interrupted jobs failed count={}",
            result.rows_affected()
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let pool = initialize_database().await?;
    let config = load_config()?;
    let app_state = AppState {
        pool,
        config,
        http: reqwest::Client::new(),
        sync_semaphore: Arc::new(Semaphore::new(MAX_GLOBAL_CONCURRENT_SYNCS)),
    };
    tokio::spawn(processor_loop(app_state.clone()));
    tokio::spawn(scheduler_loop(app_state.clone()));
    let app = Router::new()
        .route("/static/style.css", get(style_css))
        .route("/login", get(login))
        .route("/login/google", get(google_start))
        .route("/login/google/callback", get(google_callback))
        .route("/logout", post(logout))
        .route("/sync", post(sync_gmail))
        .route("/partials/inbox-table", get(inbox_table))
        .route("/partials/sync-button", get(sync_button))
        .route("/", get(inbox))
        .route("/threads/{id}", get(thread))
        .route("/threads/{id}/submit", post(submit_thread_form))
        .route("/settings", get(settings).post(save_settings))
        .route("/settings/reset-data", post(reset_synced_data))
        .with_state(app_state);

    let address = SocketAddr::from(([127, 0, 0, 1], 3000));
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("listening on http://{address}");
    axum::serve(listener, app).await?;

    Ok(())
}
