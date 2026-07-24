use sqlx::SqlitePool;

use crate::{
    domain::{FilterOption, ThreadView},
    repository::{fetch_thread_views, sync_job_is_running},
};

pub async fn inbox_threads(
    pool: &SqlitePool,
    user_id: i64,
    selected_state: Option<&str>,
) -> Result<Vec<ThreadView>, sqlx::Error> {
    fetch_thread_views(pool, user_id, selected_state).await
}

pub async fn sync_button_is_syncing(pool: &SqlitePool, user_id: i64) -> Result<bool, sqlx::Error> {
    sync_job_is_running(pool, user_id).await
}

pub fn filters(selected: &str) -> Vec<FilterOption<'_>> {
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
