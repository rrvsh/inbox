use axum::{extract::State, http::HeaderMap, response::Redirect};

use crate::{
    app::AppState,
    error::AppError,
    repository::{active_sync_job_exists, create_sync_job, require_user_session},
    services::gmail_sync,
};

pub async fn sync_gmail(
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
        if let Err(error) =
            gmail_sync::run_sync_job(worker_state, session.user_id, session.email, job_id).await
        {
            eprintln!("sync_job: failed job_id={job_id} error={}", error.0);
        }
    });
    Ok(Redirect::to("/"))
}
