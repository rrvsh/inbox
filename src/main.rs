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
use std::{env, error::Error, net::SocketAddr, str::FromStr};

const SESSION_COOKIE: &str = "inbox_session";

#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
    config: AppConfig,
    http: reqwest::Client,
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

struct UserSession {
    user_id: i64,
    email: String,
    access_token: String,
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

#[derive(FromRow)]
struct MessageRow {
    sender: String,
    body: String,
    sent_at: String,
}

#[derive(FromRow)]
struct UserFormRow {
    title: String,
    body: String,
}

#[derive(FromRow)]
struct ActionLogRow {
    actor: String,
    message: String,
    created_at: String,
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
    response: String,
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
    threads: Vec<ThreadRow>,
    filters: Vec<FilterOption<'a>>,
    user_email: String,
}

#[derive(Template)]
#[template(path = "thread.html")]
struct ThreadTemplate {
    thread: ThreadRow,
    messages: Vec<MessageRow>,
    form: Option<UserFormRow>,
    logs: Vec<ActionLogRow>,
    user_email: String,
}

fn render(template: impl Template) -> Result<Response, AppError> {
    let html = template.render()?;
    Ok(Html(html).into_response())
}

async fn login() -> Result<Response, AppError> {
    render(LoginTemplate)
}

async fn google_start(State(state): State<AppState>) -> Result<Response, AppError> {
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
    let token = exchange_google_code(&state, &code).await?;
    let email = fetch_gmail_profile_email(&state, &token.access_token).await?;
    let user_id = upsert_user(&state.pool, &email, &token).await?;
    sync_latest_threads(
        &state,
        &UserSession {
            user_id,
            email: email.clone(),
            access_token: token.access_token.clone(),
        },
    )
    .await?;
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
    let selected_state = query.state.as_deref().filter(|value| *value != "all");
    let threads = if let Some(selected_state) = selected_state {
        sqlx::query_as::<_, ThreadRow>(
            "SELECT id, subject, from_display, state, latest_action, waiting_until FROM threads WHERE user_id = ? AND state = ? ORDER BY updated_at DESC",
        )
        .bind(session.user_id)
        .bind(selected_state)
        .fetch_all(&state.pool)
        .await?
    } else {
        sqlx::query_as::<_, ThreadRow>(
            "SELECT id, subject, from_display, state, latest_action, waiting_until FROM threads WHERE user_id = ? ORDER BY updated_at DESC",
        )
        .bind(session.user_id)
        .fetch_all(&state.pool)
        .await?
    };

    render(InboxTemplate {
        threads,
        filters: filters(query.state.as_deref().unwrap_or("all")),
        user_email: session.email,
    })
}

async fn settings(headers: HeaderMap, State(state): State<AppState>) -> Result<Response, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login").into_response());
    };

    render(PageTemplate {
        title: "Settings",
        heading: "Settings",
        body: "OpenRouter API key settings will be wired here.",
        user_email: &session.email,
    })
}

async fn sync_gmail(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Redirect, AppError> {
    let Some(session) = require_user_session(&headers, &state.pool).await? else {
        return Ok(Redirect::to("/login"));
    };

    sync_latest_threads(&state, &session).await?;
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
        "SELECT title, body FROM user_forms WHERE thread_id = ? AND status = 'active' ORDER BY id DESC LIMIT 1",
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

    render(ThreadTemplate {
        thread,
        messages,
        form,
        logs,
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

    sqlx::query(
        "UPDATE user_forms SET status = 'submitted' WHERE thread_id = ? AND status = 'active'",
    )
    .bind(id)
    .execute(&state.pool)
    .await?;
    sqlx::query(
        "UPDATE threads SET state = 'Resolved', latest_action = 'User submitted response', updated_at = '2026-07-23T00:00:00Z' WHERE id = ?",
    )
    .bind(id)
    .execute(&state.pool)
    .await?;
    sqlx::query(
        "INSERT INTO action_logs (thread_id, actor, message, created_at) VALUES (?, 'user', ?, '2026-07-23T00:00:00Z')",
    )
    .bind(id)
    .bind(format!("Submitted response: {}", submission.response))
    .execute(&state.pool)
    .await?;
    sqlx::query(
        "INSERT INTO action_logs (thread_id, actor, message, created_at) VALUES (?, 'system', 'State changed to Resolved', '2026-07-23T00:00:01Z')",
    )
    .bind(id)
    .execute(&state.pool)
    .await?;

    Ok(Redirect::to(&format!("/threads/{id}")))
}

fn filters(selected: &str) -> Vec<FilterOption<'_>> {
    [
        ("NeedsAction", "Needs Action"),
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

async fn sync_latest_threads(state: &AppState, session: &UserSession) -> Result<(), AppError> {
    let response = state
        .http
        .get("https://gmail.googleapis.com/gmail/v1/users/me/threads")
        .bearer_auth(&session.access_token)
        .query(&[("maxResults", "10")])
        .send()
        .await?
        .error_for_status()?;
    let threads = response.json::<GmailThreadsResponse>().await?;

    for thread in threads.threads.unwrap_or_default() {
        let gmail_thread = fetch_gmail_thread(state, session, &thread.id).await?;
        upsert_gmail_thread(&state.pool, session.user_id, &gmail_thread).await?;
    }

    Ok(())
}

async fn fetch_gmail_thread(
    state: &AppState,
    session: &UserSession,
    gmail_thread_id: &str,
) -> Result<GmailThreadResponse, AppError> {
    let response = state
        .http
        .get(format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/threads/{gmail_thread_id}"
        ))
        .bearer_auth(&session.access_token)
        .query(&[("format", "full")])
        .send()
        .await?
        .error_for_status()?;
    Ok(response.json::<GmailThreadResponse>().await?)
}

async fn upsert_gmail_thread(
    pool: &SqlitePool,
    user_id: i64,
    gmail_thread: &GmailThreadResponse,
) -> Result<(), sqlx::Error> {
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
         VALUES (?, ?, ?, ?, 'Processing', 'Synced from Gmail', NULL, CURRENT_TIMESTAMP, ?, ?)
         ON CONFLICT(user_id, gmail_thread_id) DO UPDATE SET
           subject = excluded.subject,
           from_display = excluded.from_display,
           latest_action = 'Synced from Gmail',
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

async fn fetch_gmail_profile_email(
    state: &AppState,
    access_token: &str,
) -> Result<String, AppError> {
    let response = state
        .http
        .get("https://gmail.googleapis.com/gmail/v1/users/me/profile")
        .bearer_auth(access_token)
        .send()
        .await?
        .error_for_status()?;
    let profile = response.json::<GmailProfileResponse>().await?;
    Ok(profile.email_address)
}

async fn upsert_user(
    pool: &SqlitePool,
    email: &str,
    token: &GoogleTokenResponse,
) -> Result<i64, sqlx::Error> {
    sqlx::query(
        "INSERT INTO users (google_email, access_token, refresh_token, expires_in, updated_at)
         VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP)
         ON CONFLICT(google_email) DO UPDATE SET
           access_token = excluded.access_token,
           refresh_token = COALESCE(excluded.refresh_token, users.refresh_token),
           expires_in = excluded.expires_in,
           updated_at = CURRENT_TIMESTAMP",
    )
    .bind(email)
    .bind(&token.access_token)
    .bind(&token.refresh_token)
    .bind(token.expires_in)
    .execute(pool)
    .await?;

    let (user_id,): (i64,) = sqlx::query_as("SELECT id FROM users WHERE google_email = ?")
        .bind(email)
        .fetch_one(pool)
        .await?;
    Ok(user_id)
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
    let row = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT users.id, users.google_email, users.access_token FROM sessions JOIN users ON users.id = sessions.user_id WHERE sessions.token = ?",
    )
    .bind(token)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(user_id, email, access_token)| UserSession {
        user_id,
        email,
        access_token,
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
    Ok(pool)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let pool = initialize_database().await?;
    let config = load_config()?;
    let app_state = AppState {
        pool,
        config,
        http: reqwest::Client::new(),
    };
    let app = Router::new()
        .route("/login", get(login))
        .route("/login/google", get(google_start))
        .route("/login/google/callback", get(google_callback))
        .route("/logout", post(logout))
        .route("/sync", post(sync_gmail))
        .route("/", get(inbox))
        .route("/threads/{id}", get(thread))
        .route("/threads/{id}/submit", post(submit_thread_form))
        .route("/settings", get(settings))
        .with_state(app_state);

    let address = SocketAddr::from(([127, 0, 0, 1], 3000));
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("listening on http://{address}");
    axum::serve(listener, app).await?;

    Ok(())
}
