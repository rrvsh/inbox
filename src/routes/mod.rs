pub mod auth;
pub mod forms;
pub mod inbox;
pub mod settings;
pub mod sync;
pub mod thread;

use axum::{
    Router,
    routing::{get, post},
};

use crate::{app::AppState, templates::style_css};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/static/style.css", get(style_css))
        .route("/login", get(auth::login))
        .route("/login/google", get(auth::google_start))
        .route("/login/google/callback", get(auth::google_callback))
        .route("/logout", post(auth::logout))
        .route("/sync", post(sync::sync_gmail))
        .route("/partials/inbox-table", get(inbox::inbox_table))
        .route("/partials/sync-button", get(inbox::sync_button))
        .route("/", get(inbox::inbox))
        .route("/threads/{id}", get(thread::thread))
        .route("/threads/{id}/submit", post(thread::submit_thread_form))
        .route(
            "/settings",
            get(settings::settings).post(settings::save_settings),
        )
        .route("/settings/reset-data", post(settings::reset_synced_data))
        .with_state(state)
}
