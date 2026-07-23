use askama::Template;
use axum::{
    Router,
    extract::{Form, Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use sqlx::{FromRow, SqlitePool, sqlite::SqliteConnectOptions};
use std::{error::Error, net::SocketAddr, str::FromStr};

#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
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
struct UserSubmission {
    response: String,
}

#[derive(Template)]
#[template(path = "page.html")]
struct PageTemplate<'a> {
    title: &'a str,
    heading: &'a str,
    body: &'a str,
}

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
    render(PageTemplate {
        title: "Login",
        heading: "Login with Google",
        body: "Google OAuth will be wired here.",
    })
}

async fn inbox(
    State(state): State<AppState>,
    Query(query): Query<InboxQuery>,
) -> Result<Response, AppError> {
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

async fn settings() -> Result<Response, AppError> {
    render(PageTemplate {
        title: "Settings",
        heading: "Settings",
        body: "OpenRouter API key settings will be wired here.",
    })
}

async fn thread(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Response, AppError> {
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
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(submission): Form<UserSubmission>,
) -> Result<Redirect, AppError> {
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
    let app_state = AppState { pool };
    let app = Router::new()
        .route("/login", get(login))
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
