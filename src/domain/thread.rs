use sqlx::FromRow;

#[derive(FromRow)]
pub struct ThreadRow {
    pub id: i64,
    pub subject: String,
    pub from_display: String,
    pub state: String,
    pub latest_action: String,
    pub waiting_until: Option<String>,
}

pub struct ThreadView {
    pub id: i64,
    pub subject: String,
    pub from_display: String,
    pub state_label: &'static str,
    pub state_class: &'static str,
    pub latest_action: String,
    pub waiting_until: Option<String>,
}

impl From<ThreadRow> for ThreadView {
    fn from(row: ThreadRow) -> Self {
        Self {
            state_label: state_label(&row.state),
            state_class: state_class(&row.state),
            id: row.id,
            subject: row.subject,
            from_display: row.from_display,
            latest_action: row.latest_action,
            waiting_until: row.waiting_until,
        }
    }
}

pub struct FilterOption<'a> {
    pub value: &'a str,
    pub label: &'a str,
    pub selected: bool,
}

pub fn state_label(state: &str) -> &'static str {
    match state {
        "NeedsAction" => "Needs Action",
        "Unprocessed" => "Unprocessed",
        "Processing" => "Processing",
        "Waiting" => "Waiting",
        "Resolved" => "Resolved",
        _ => "Unknown",
    }
}

pub fn state_class(state: &str) -> &'static str {
    match state {
        "NeedsAction" => "needs-action",
        "Unprocessed" => "unprocessed",
        "Processing" => "processing",
        "Waiting" => "waiting",
        "Resolved" => "resolved",
        _ => "unknown",
    }
}
