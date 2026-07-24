use chrono::{DateTime, Datelike, Local, Utc};
use sqlx::FromRow;

#[derive(FromRow)]
pub struct MessageRow {
    pub sender: String,
    pub body: String,
    pub sent_at: String,
}

pub struct MessageView {
    pub sender: String,
    pub body: String,
    pub sent_at_display: String,
}

impl From<MessageRow> for MessageView {
    fn from(row: MessageRow) -> Self {
        Self {
            sender: row.sender,
            body: row.body,
            sent_at_display: format_gmail_timestamp_local(&row.sent_at),
        }
    }
}

fn format_gmail_timestamp_local(value: &str) -> String {
    let Ok(millis) = value.parse::<i64>() else {
        return value.to_owned();
    };
    DateTime::<Utc>::from_timestamp_millis(millis).map_or_else(
        || value.to_owned(),
        |datetime| {
            let local = datetime.with_timezone(&Local);
            format!(
                "{} {}, {}",
                ordinal_day(local.day()),
                local.format("%B %Y"),
                local.format("%-I.%M%P")
            )
        },
    )
}

fn ordinal_day(day: u32) -> String {
    let suffix = match day % 100 {
        11..=13 => "th",
        _ => match day % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    };
    format!("{day}{suffix}")
}
