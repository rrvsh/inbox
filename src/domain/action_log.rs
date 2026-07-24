use sqlx::FromRow;

#[derive(FromRow)]
pub struct ActionLogRow {
    pub actor: String,
    pub message: String,
    pub created_at: String,
}
