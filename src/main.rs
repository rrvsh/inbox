use askama::Template;
use axum::{
    Router,
    extract::{Form, Path, Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
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
    state: Option<String>,
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

#[derive(Template)]
#[template(path = "page.html")]
struct PageTemplate<'a> {
    title: &'a str,
    heading: &'a str,
    body: &'a str,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate;

#[derive(Template)]
#[template(path = "inbox.html")]
struct InboxTemplate<'a> {
    threads: Vec<ThreadRow>,
    filters: Vec<FilterOption<'a>>,
}

#[derive(Template)]
#[template(path = "thread.html")]
struct ThreadTemplate {
    thread: ThreadRow,
    messages: Vec<MessageRow>,
    form: Option<UserFormRow>,
    logs: Vec<ActionLogRow>,
}

fn render(template: impl Template) -> Result<Response, AppError> {
    let html = template.render()?;
    Ok(Html(html).into_response())
}

async fn login() -> Result<Response, AppError> {
    render(LoginTemplate)
}

async fn google_start(State(state): State<AppState>) -> Result<Response, AppError> {
    let oauth_state = random_token(&state.pool).await?;
    sqlx::query("INSERT INTO oauth_states (state) VALUES (?)")
        .bind(&oauth_state)
        .execute(&state.pool)
        .await?;

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
        .append_pair("include_granted_scopes", "true")
        .append_pair("state", &oauth_state);

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
    let oauth_state = query
        .state
        .ok_or_else(|| AppError("missing OAuth state".to_owned()))?;
    let deleted = sqlx::query("DELETE FROM oauth_states WHERE state = ?")
        .bind(&oauth_state)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if deleted != 1 {
        return Ok((axum::http::StatusCode::BAD_REQUEST, "invalid OAuth state").into_response());
    }

    let token = exchange_google_code(&state, &code).await?;
    let email = fetch_gmail_profile_email(&state, &token.access_token).await?;
    let user_id = upsert_user(&state.pool, &email, &token).await?;
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
    if require_user_id(&headers, &state.pool).await?.is_none() {
        return Ok(Redirect::to("/login").into_response());
    }

    let selected_state = query.state.as_deref().filter(|value| *value != "all");
    let threads = if let Some(selected_state) = selected_state {
        sqlx::query_as::<_, ThreadRow>(
            "SELECT id, subject, from_display, state, latest_action, waiting_until FROM threads WHERE state = ? ORDER BY updated_at DESC",
        )
        .bind(selected_state)
        .fetch_all(&state.pool)
        .await?
    } else {
        sqlx::query_as::<_, ThreadRow>(
            "SELECT id, subject, from_display, state, latest_action, waiting_until FROM threads ORDER BY updated_at DESC",
        )
        .fetch_all(&state.pool)
        .await?
    };

    render(InboxTemplate {
        threads,
        filters: filters(query.state.as_deref().unwrap_or("all")),
    })
}

async fn settings(headers: HeaderMap, State(state): State<AppState>) -> Result<Response, AppError> {
    if require_user_id(&headers, &state.pool).await?.is_none() {
        return Ok(Redirect::to("/login").into_response());
    }

    render(PageTemplate {
        title: "Settings",
        heading: "Settings",
        body: "OpenRouter API key settings will be wired here.",
    })
}

async fn thread(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    if require_user_id(&headers, &state.pool).await?.is_none() {
        return Ok(Redirect::to("/login").into_response());
    }

    let thread = sqlx::query_as::<_, ThreadRow>(
        "SELECT id, subject, from_display, state, latest_action, waiting_until FROM threads WHERE id = ?",
    )
    .bind(id)
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
    })
}

async fn submit_thread_form(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(submission): Form<UserSubmission>,
) -> Result<Redirect, AppError> {
    if require_user_id(&headers, &state.pool).await?.is_none() {
        return Ok(Redirect::to("/login"));
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

async fn require_user_id(
    headers: &HeaderMap,
    pool: &SqlitePool,
) -> Result<Option<i64>, sqlx::Error> {
    let Some(token) = session_token_from_headers(headers) else {
        return Ok(None);
    };
    let row = sqlx::query_as::<_, (i64,)>("SELECT user_id FROM sessions WHERE token = ?")
        .bind(token)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(user_id,)| user_id))
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
    format!("{}/auth/google/callback", config.app_base_url)
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
    seed_mock_data(&pool).await?;
    Ok(pool)
}

async fn seed_mock_data(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let (thread_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM threads")
        .fetch_one(pool)
        .await?;
    if thread_count > 0 {
        return Ok(());
    }

    sqlx::query(
        "INSERT INTO threads (subject, from_display, state, latest_action, waiting_until, updated_at) VALUES
        ('Quote for ACME renewal', 'Sam at ACME', 'NeedsAction', 'Draft reply ready', NULL, '2026-07-23T10:00:00Z'),
        ('Lunch next week', 'Mina', 'Waiting', 'Waiting for Mina to pick a day', '2026-07-25', '2026-07-22T09:00:00Z'),
        ('Welcome to the beta', 'Product Team', 'Resolved', 'Classifier marked resolved', NULL, '2026-07-21T08:00:00Z')",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO messages (thread_id, sender, body, sent_at) VALUES
        (1, 'Sam at ACME', 'Can you send over the updated renewal quote?', '2026-07-23T09:30:00Z'),
        (1, 'You', 'I will take a look and get back to you.', '2026-07-23T09:45:00Z'),
        (2, 'Mina', 'Want to do lunch next week?', '2026-07-22T09:00:00Z'),
        (3, 'Product Team', 'Thanks for joining the beta.', '2026-07-21T08:00:00Z')",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO user_forms (thread_id, form_type, title, body, status, created_at) VALUES
        (1, 'DraftReply', 'Review draft reply', 'Hi Sam, happy to send the updated quote. I will confirm the numbers and follow up shortly.', 'active', '2026-07-23T10:00:00Z')",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO action_logs (thread_id, actor, message, created_at) VALUES
        (1, 'agent', 'Classifier requested a drafted reply', '2026-07-23T09:59:00Z'),
        (1, 'agent', 'Reply drafter created a draft reply form', '2026-07-23T10:00:00Z'),
        (2, 'agent', 'Classifier set state to Waiting', '2026-07-22T09:01:00Z'),
        (3, 'agent', 'Classifier marked thread Resolved', '2026-07-21T08:01:00Z')",
    )
    .execute(pool)
    .await?;

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
    };
    let app = Router::new()
        .route("/login", get(login))
        .route("/auth/google/start", get(google_start))
        .route("/auth/google/callback", get(google_callback))
        .route("/logout", post(logout))
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
