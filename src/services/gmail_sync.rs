use std::{
    collections::HashSet,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    app::AppState,
    domain::SyncUser,
    error::AppError,
    integrations::{
        gmail::{
            GmailHistory, GmailHistoryMessage, GmailHistoryResponse, GmailThreadResponse,
            GmailThreadStub, GmailThreadsResponse, GoogleTokenResponse, fetch_gmail_profile,
        },
        openrouter::preview,
    },
    repository::{
        create_sync_job, due_sync_users, load_sync_user, log_thread_event, log_thread_state_counts,
        mark_gmail_reauth_required, update_sync_job, upsert_gmail_thread, user_has_threads,
    },
};

const TOKEN_REFRESH_SKEW_SECONDS: i64 = 300;
const MAX_CHANGED_THREADS_PER_SYNC: usize = 25;
const MAX_HISTORY_PAGES_PER_SYNC: usize = 5;
const SCHEDULER_TICK_SECONDS: u64 = 30;
pub async fn run_sync_job(
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

pub async fn scheduler_loop(state: AppState) {
    eprintln!("sync_scheduler: loop started");
    loop {
        if let Err(error) = enqueue_due_sync_jobs(&state).await {
            eprintln!("sync_scheduler: error={}", error.0);
        }
        tokio::time::sleep(std::time::Duration::from_secs(SCHEDULER_TICK_SECONDS)).await;
    }
}

pub async fn enqueue_due_sync_jobs(state: &AppState) -> Result<(), AppError> {
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

pub async fn sync_gmail_changes(state: &AppState, user_id: i64) -> Result<(), AppError> {
    let mut sync_user = load_sync_user(&state.pool, user_id).await?;
    if sync_user.gmail_reauth_required {
        return Err(AppError("Reconnect Gmail before syncing".to_owned()));
    }
    let access_token = ensure_gmail_token(state, &mut sync_user).await?;
    let use_incremental =
        sync_user.gmail_history_id.is_some() && user_has_threads(&state.pool, user_id).await?;
    let result = if use_incremental {
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
        let use_incremental =
            sync_user.gmail_history_id.is_some() && user_has_threads(&state.pool, user_id).await?;
        if use_incremental {
            sync_incremental_history(state, &sync_user, &access_token).await?;
        } else {
            bootstrap_latest_threads(state, &sync_user, &access_token).await?;
        }
    }
    log_thread_state_counts(&state.pool, user_id, "gmail_sync_complete").await?;
    Ok(())
}

pub async fn bootstrap_latest_threads(
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
    fetch_and_upsert_threads(state, sync_user, access_token, &threads).await?;
    sqlx::query(
        "UPDATE users SET gmail_history_id = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(history_id)
    .bind(sync_user.user_id)
    .execute(&state.pool)
    .await?;
    Ok(())
}

pub async fn sync_incremental_history(
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
    fetch_and_upsert_threads(state, sync_user, access_token, &stubs).await?;

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

pub fn is_gmail_unauthorized(error: &AppError) -> bool {
    error.0.contains("status=401") || error.0.contains("401 Unauthorized")
}

pub fn collect_changed_thread_ids(
    history: &[GmailHistory],
    changed_thread_ids: &mut HashSet<String>,
) {
    for entry in history {
        collect_history_messages(entry.messages_added.as_deref(), changed_thread_ids);
        collect_history_messages(entry.messages_deleted.as_deref(), changed_thread_ids);
        collect_history_messages(entry.labels_added.as_deref(), changed_thread_ids);
        collect_history_messages(entry.labels_removed.as_deref(), changed_thread_ids);
    }
}

pub fn collect_history_messages(
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

pub async fn fetch_and_upsert_threads(
    state: &AppState,
    sync_user: &SyncUser,
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
        let thread_id = upsert_gmail_thread(
            &state.pool,
            sync_user.user_id,
            &sync_user.email,
            &gmail_thread,
        )
        .await?;
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

pub async fn fetch_gmail_thread(
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

pub async fn ensure_gmail_token(
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

pub async fn refresh_gmail_token(
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

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}
