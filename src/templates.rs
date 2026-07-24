use askama::Template;
use axum::{
    http::header,
    response::{Html, IntoResponse, Response},
};

use crate::{
    domain::{ActionLogRow, ChatRow, FilterOption, MessageRow, ThreadView, UserFormRow},
    error::AppError,
};

#[derive(Template)]
#[template(path = "page.html")]
pub struct PageTemplate<'a> {
    pub title: &'a str,
    pub heading: &'a str,
    pub body: &'a str,
    pub user_email: &'a str,
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginTemplate;

#[derive(Template)]
#[template(path = "inbox.html")]
pub struct InboxTemplate<'a> {
    pub threads: Vec<ThreadView>,
    pub filters: Vec<FilterOption<'a>>,
    pub user_email: String,
    pub syncing: bool,
}

#[derive(Template)]
#[template(path = "inbox_table.html")]
pub struct InboxTableTemplate {
    pub threads: Vec<ThreadView>,
}

#[derive(Template)]
#[template(path = "sync_button.html")]
pub struct SyncButtonTemplate {
    pub syncing: bool,
}

#[derive(Template)]
#[template(path = "thread.html")]
pub struct ThreadTemplate {
    pub thread: ThreadView,
    pub messages: Vec<MessageRow>,
    pub form: Option<UserFormRow>,
    pub logs: Vec<ActionLogRow>,
    pub chats: Vec<ChatRow>,
    pub user_email: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsTemplate {
    pub user_email: String,
    pub openrouter_key: String,
}

pub fn render(template: impl Template) -> Result<Response, AppError> {
    let html = template.render()?;
    Ok(Html(html).into_response())
}

pub async fn style_css() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../static/style.css"),
    )
        .into_response()
}
