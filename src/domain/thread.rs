use chrono::{DateTime, Local, Utc};
use sqlx::FromRow;

#[derive(FromRow)]
pub struct ThreadRow {
    pub id: i64,
    pub subject: String,
    pub from_display: String,
    pub state: String,
    pub latest_action: String,
    pub waiting_until: Option<String>,
    pub last_message_at: Option<String>,
    pub unread: bool,
}

pub struct ThreadView {
    pub id: i64,
    pub subject: String,
    pub from_display: String,
    pub state_label: &'static str,
    pub state_class: &'static str,
    pub status_display: String,
    pub latest_action: String,
    pub next_action: String,
    pub last_message_at_display: String,
    pub unread: bool,
}

impl From<ThreadRow> for ThreadView {
    fn from(row: ThreadRow) -> Self {
        Self {
            state_label: state_label(&row.state),
            state_class: state_class(&row.state),
            id: row.id,
            subject: row.subject,
            status_display: status_display(&row.state, row.waiting_until.as_deref()),
            next_action: next_action(&row.state, &row.latest_action),
            last_message_at_display: format_relative_gmail_timestamp(
                row.last_message_at.as_deref(),
            ),
            unread: row.unread,
            from_display: row.from_display,
            latest_action: row.latest_action,
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

fn status_display(state: &str, waiting_until: Option<&str>) -> String {
    if state == "Waiting"
        && let Some(waiting_until) = waiting_until.filter(|value| !value.is_empty())
    {
        return format!("Waiting · {waiting_until}");
    }
    state_label(state).to_owned()
}

fn next_action(state: &str, latest_action: &str) -> String {
    match state {
        "NeedsAction" if latest_action.to_lowercase().contains("draft") => {
            "Review draft".to_owned()
        }
        "NeedsAction" if latest_action.to_lowercase().contains("resolve") => {
            "Confirm resolved".to_owned()
        }
        "NeedsAction" => "Respond".to_owned(),
        "Waiting" => "Waiting for reply".to_owned(),
        "Processing" | "Unprocessed" => "Processing".to_owned(),
        "Resolved" => "Resolved".to_owned(),
        _ => latest_action.to_owned(),
    }
}

fn format_relative_gmail_timestamp(value: Option<&str>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let Ok(millis) = value.parse::<i64>() else {
        return value.to_owned();
    };
    DateTime::<Utc>::from_timestamp_millis(millis).map_or_else(
        || value.to_owned(),
        |datetime| {
            let now = Utc::now();
            let age = now.signed_duration_since(datetime);
            if age.num_seconds() < 60 {
                format!("{}s ago", age.num_seconds().max(0))
            } else if age.num_minutes() < 60 {
                format!("{}m ago", age.num_minutes())
            } else if age.num_hours() < 24 {
                format!("{}h ago", age.num_hours())
            } else if age.num_days() <= 10 {
                format!("{}d ago", age.num_days())
            } else {
                datetime
                    .with_timezone(&Local)
                    .format("%Y-%m-%d")
                    .to_string()
            }
        },
    )
}
