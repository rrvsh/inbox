use sqlx::SqlitePool;

const SYNC_INTERVAL_SECONDS: u64 = 300;
const MAX_ENQUEUES_PER_TICK: i64 = 10;

pub async fn create_sync_job(pool: &SqlitePool, user_id: i64) -> Result<i64, sqlx::Error> {
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

pub async fn update_sync_job(
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

pub async fn sync_job_is_running(pool: &SqlitePool, user_id: i64) -> Result<bool, sqlx::Error> {
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

pub async fn active_sync_job_exists(pool: &SqlitePool, user_id: i64) -> Result<bool, sqlx::Error> {
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

pub async fn due_sync_users(pool: &SqlitePool) -> Result<Vec<(i64, String)>, sqlx::Error> {
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

pub async fn fail_interrupted_sync_jobs(pool: &SqlitePool) -> Result<(), sqlx::Error> {
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
