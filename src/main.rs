#![allow(
    clippy::implicit_hasher,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

pub mod app;
pub mod domain;
pub mod error;
pub mod integrations;
pub mod repository;
pub mod routes;
pub mod services;
pub mod templates;

use std::{error::Error, net::SocketAddr, str::FromStr, sync::Arc};

use app::AppState;
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use tokio::sync::Semaphore;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let options = SqliteConnectOptions::from_str("sqlite:inbox.db")?.create_if_missing(true);
    let pool = SqlitePool::connect_with(options).await?;
    sqlx::migrate!().run(&pool).await?;
    repository::fail_interrupted_sync_jobs(&pool).await?;

    let app_state = AppState {
        pool,
        config: app::load_config()?,
        http: reqwest::Client::new(),
        sync_semaphore: Arc::new(Semaphore::new(5)),
    };

    tokio::spawn(services::workflow::processor_loop(app_state.clone()));
    tokio::spawn(services::gmail_sync::scheduler_loop(app_state.clone()));

    let address = SocketAddr::from(([127, 0, 0, 1], 3000));
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("listening on http://{address}");
    axum::serve(listener, routes::router(app_state)).await?;

    Ok(())
}
