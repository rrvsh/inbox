use base64::{Engine, engine::general_purpose::URL_SAFE};
use serde::Deserialize;

use crate::{app::AppState, error::AppError};

#[derive(Deserialize)]
pub struct GoogleTokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
}

#[derive(Deserialize)]
pub struct GmailProfileResponse {
    #[serde(rename = "emailAddress")]
    pub email_address: String,
    #[serde(rename = "historyId")]
    pub history_id: Option<String>,
}

#[derive(Deserialize)]
pub struct GmailHistoryResponse {
    pub history: Option<Vec<GmailHistory>>,
    #[serde(rename = "nextPageToken")]
    pub next_page_token: Option<String>,
    #[serde(rename = "historyId")]
    pub history_id: Option<String>,
}

#[derive(Deserialize)]
pub struct GmailHistory {
    #[serde(rename = "messagesAdded")]
    pub messages_added: Option<Vec<GmailHistoryMessage>>,
    #[serde(rename = "messagesDeleted")]
    pub messages_deleted: Option<Vec<GmailHistoryMessage>>,
    #[serde(rename = "labelsAdded")]
    pub labels_added: Option<Vec<GmailHistoryMessage>>,
    #[serde(rename = "labelsRemoved")]
    pub labels_removed: Option<Vec<GmailHistoryMessage>>,
}

#[derive(Deserialize)]
pub struct GmailHistoryMessage {
    pub message: GmailHistoryMessageRef,
}

#[derive(Deserialize)]
pub struct GmailHistoryMessageRef {
    #[serde(rename = "threadId")]
    pub thread_id: Option<String>,
}

#[derive(Deserialize)]
pub struct GmailThreadsResponse {
    pub threads: Option<Vec<GmailThreadStub>>,
}

#[derive(Deserialize)]
pub struct GmailThreadStub {
    pub id: String,
}

#[derive(Deserialize)]
pub struct GmailThreadResponse {
    pub id: String,
    pub snippet: Option<String>,
    pub messages: Vec<GmailMessage>,
}

#[derive(Deserialize)]
pub struct GmailMessage {
    pub id: String,
    #[serde(rename = "internalDate")]
    pub internal_date: Option<String>,
    pub payload: GmailPayload,
}

#[derive(Deserialize)]
pub struct GmailPayload {
    pub headers: Vec<GmailHeader>,
    pub body: Option<GmailBody>,
    pub parts: Option<Vec<Self>>,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
}

#[derive(Deserialize)]
pub struct GmailHeader {
    pub name: String,
    pub value: String,
}

#[derive(Deserialize)]
pub struct GmailBody {
    pub data: Option<String>,
}

pub async fn exchange_google_code(
    state: &AppState,
    code: &str,
    redirect_uri: &str,
) -> Result<GoogleTokenResponse, AppError> {
    let params = [
        ("code", code),
        ("client_id", state.config.google_client_id.as_str()),
        ("client_secret", state.config.google_client_secret.as_str()),
        ("redirect_uri", redirect_uri),
        ("grant_type", "authorization_code"),
    ];
    let response = state
        .http
        .post("https://oauth2.googleapis.com/token")
        .form(&params)
        .send()
        .await?
        .error_for_status()?;
    Ok(response.json::<GoogleTokenResponse>().await?)
}

pub async fn fetch_gmail_profile(
    state: &AppState,
    access_token: &str,
) -> Result<GmailProfileResponse, AppError> {
    let response = state
        .http
        .get("https://gmail.googleapis.com/gmail/v1/users/me/profile")
        .bearer_auth(access_token)
        .send()
        .await?
        .error_for_status()?;
    Ok(response.json::<GmailProfileResponse>().await?)
}

pub async fn fetch_gmail_profile_email(
    state: &AppState,
    access_token: &str,
) -> Result<String, AppError> {
    let profile = fetch_gmail_profile(state, access_token).await?;
    Ok(profile.email_address)
}

pub fn header_value<'a>(headers: &'a [GmailHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}

pub fn extract_body_text(payload: &GmailPayload) -> Option<String> {
    let mime_type = payload.mime_type.as_deref();
    if matches!(mime_type, Some("text/plain" | "text/html"))
        && let Some(data) = payload.body.as_ref().and_then(|body| body.data.as_ref())
    {
        return decode_gmail_body(data);
    }

    payload.parts.as_ref().and_then(|parts| {
        parts
            .iter()
            .find_map(|part| extract_body_text(part).filter(|body| !body.is_empty()))
    })
}

fn decode_gmail_body(data: &str) -> Option<String> {
    let padded = match data.len() % 4 {
        0 => data.to_owned(),
        remainder => format!("{}{}", data, "=".repeat(4 - remainder)),
    };
    URL_SAFE
        .decode(padded)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}
