use sqlx::FromRow;

#[derive(FromRow)]
pub struct UserFormRow {
    pub form_type: String,
    pub title: String,
    pub body: String,
}
