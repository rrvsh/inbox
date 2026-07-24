use sqlx::FromRow;

#[derive(FromRow)]
pub struct ChatRow {
    pub role: String,
    pub message: String,
}
