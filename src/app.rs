use std::{env, error::Error, sync::Arc};

use sqlx::SqlitePool;
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: AppConfig,
    pub http: reqwest::Client,
    pub sync_semaphore: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct AppConfig {
    pub google_client_id: String,
    pub google_client_secret: String,
    pub app_base_url: String,
}

pub fn load_config() -> Result<AppConfig, Box<dyn Error>> {
    dotenvy::dotenv().ok();
    Ok(AppConfig {
        google_client_id: env::var("GOOGLE_CLIENT_ID")?,
        google_client_secret: env::var("GOOGLE_CLIENT_SECRET")?,
        app_base_url: env::var("APP_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:3000".to_owned()),
    })
}
