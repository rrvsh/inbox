use sqlx::FromRow;

#[derive(FromRow)]
pub struct MessageRow {
    pub sender: String,
    pub body: String,
    pub sent_at: String,
}
