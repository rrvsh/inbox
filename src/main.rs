use askama::Template;
use axum::{
    Router,
    extract::Path,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use std::{error::Error, net::SocketAddr};

#[derive(Template)]
#[template(path = "page.html")]
struct PageTemplate<'a> {
    title: &'a str,
    heading: &'a str,
    body: &'a str,
}

#[derive(Template)]
#[template(path = "thread.html")]
struct ThreadTemplate {
    id: u64,
}

fn render(template: impl Template) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(error) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("template render failed: {error}"),
        )
            .into_response(),
    }
}

async fn login() -> Response {
    render(PageTemplate {
        title: "Login",
        heading: "Login with Google",
        body: "Google OAuth will be wired here.",
    })
}

async fn inbox() -> Response {
    render(PageTemplate {
        title: "Inbox",
        heading: "Inbox",
        body: "Thread list will be rendered here.",
    })
}

async fn settings() -> Response {
    render(PageTemplate {
        title: "Settings",
        heading: "Settings",
        body: "OpenRouter API key settings will be wired here.",
    })
}

async fn thread(Path(id): Path<u64>) -> Response {
    render(ThreadTemplate { id })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let app = Router::new()
        .route("/login", get(login))
        .route("/", get(inbox))
        .route("/threads/{id}", get(thread))
        .route("/settings", get(settings));

    let address = SocketAddr::from(([127, 0, 0, 1], 3000));
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("listening on http://{address}");
    axum::serve(listener, app).await?;

    Ok(())
}
