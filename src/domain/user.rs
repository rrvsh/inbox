#[derive(Clone)]
pub struct UserSession {
    pub user_id: i64,
    pub email: String,
    pub openrouter_key: Option<String>,
}

pub struct SyncUser {
    pub user_id: i64,
    pub email: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_expires_at: Option<i64>,
    pub gmail_history_id: Option<String>,
    pub gmail_reauth_required: bool,
}
