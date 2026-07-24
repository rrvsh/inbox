#[derive(serde::Deserialize)]
pub struct InboxQuery {
    pub state: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct GoogleCallbackQuery {
    pub code: Option<String>,
    pub error: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct UserSubmission {
    pub action: String,
    pub response: Option<String>,
    pub draft_body: Option<String>,
    pub comment: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct SettingsSubmission {
    pub openrouter_key: String,
}
