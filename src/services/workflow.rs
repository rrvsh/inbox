use std::fmt::Write;

use serde::Deserialize;
use sqlx::SqlitePool;

use crate::{
    app::AppState,
    domain::{ChatRow, MessageRow, ThreadRow, UserSession},
    error::AppError,
    integrations::openrouter::call_openrouter_json,
    repository::{
        claim_unprocessed_thread, create_user_form, insert_thread_chat, log_thread_event,
        log_thread_state_counts, next_unprocessed_thread_id, set_thread_state, submit_active_forms,
        thread_chats, user_session_for_thread,
    },
    routes::forms::UserSubmission,
};

#[derive(Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
enum ClassifierOutput {
    NeedsReply,
    Waiting { reason: String, wake_date: String },
    AskToResolve { reason: String },
}

#[derive(Deserialize)]
struct DraftOutput {
    draft: String,
    message: Option<String>,
}

enum DraftInstruction<'a> {
    Initial,
    Revision {
        current_draft: &'a str,
        latest_comment: &'a str,
    },
}

pub async fn handle_draft_revision(
    state: &AppState,
    session: &UserSession,
    thread_id: i64,
    submission: &UserSubmission,
) -> Result<(), AppError> {
    let comment = submission.comment.as_deref().unwrap_or_default().trim();
    if !comment.is_empty() {
        insert_thread_chat(&state.pool, thread_id, "user", comment).await?;
    }
    set_thread_state(
        &state.pool,
        thread_id,
        "Processing",
        "Revising draft reply",
        None,
    )
    .await?;
    if let Err(error) = run_reply_drafter(
        state,
        session,
        thread_id,
        DraftInstruction::Revision {
            current_draft: submission.draft_body.as_deref().unwrap_or_default(),
            latest_comment: comment,
        },
    )
    .await
    {
        set_thread_state(
            &state.pool,
            thread_id,
            "NeedsAction",
            "Draft revision failed",
            None,
        )
        .await?;
        log_thread_event(
            &state.pool,
            thread_id,
            "system",
            &format!("Draft revision failed: {}", error.0),
        )
        .await?;
        return Err(error);
    }
    Ok(())
}

pub async fn handle_classifier_comment(
    state: &AppState,
    session: &UserSession,
    thread_id: i64,
    submission: &UserSubmission,
) -> Result<(), AppError> {
    let comment = submission.comment.as_deref().unwrap_or_default().trim();
    if !comment.is_empty() {
        insert_thread_chat(&state.pool, thread_id, "user", comment).await?;
    }
    submit_active_forms(&state.pool, thread_id).await?;
    set_thread_state(
        &state.pool,
        thread_id,
        "Processing",
        "Classifier running",
        None,
    )
    .await?;
    if let Err(error) = run_classifier(state, session, thread_id).await {
        set_thread_state(
            &state.pool,
            thread_id,
            "NeedsAction",
            "Classifier failed",
            None,
        )
        .await?;
        log_thread_event(
            &state.pool,
            thread_id,
            "system",
            &format!("Classifier failed after user comment: {}", error.0),
        )
        .await?;
        return Err(error);
    }
    Ok(())
}

pub async fn processor_loop(state: AppState) {
    eprintln!("processor: loop started");
    loop {
        match process_next_unprocessed_thread(&state).await {
            Ok(true) => {}
            Ok(false) => tokio::time::sleep(std::time::Duration::from_secs(3)).await,
            Err(error) => {
                eprintln!("processor: loop error={}", error.0);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
}

pub async fn process_next_unprocessed_thread(state: &AppState) -> Result<bool, AppError> {
    let Some(thread_id) = next_unprocessed_thread_id(&state.pool).await? else {
        return Ok(false);
    };
    if !claim_unprocessed_thread(&state.pool, thread_id).await? {
        return Ok(true);
    }
    eprintln!("processor: claimed thread_id={thread_id}");
    let session = user_session_for_thread(&state.pool, thread_id).await?;
    log_thread_event(&state.pool, thread_id, "system", "Processing started").await?;
    if let Err(error) = run_classifier(state, &session, thread_id).await {
        set_thread_state(
            &state.pool,
            thread_id,
            "NeedsAction",
            "Processing failed",
            None,
        )
        .await?;
        log_thread_event(
            &state.pool,
            thread_id,
            "system",
            &format!("Processing failed: {}", error.0),
        )
        .await?;
        return Err(error);
    }
    let (state_name, latest_action): (String, String) =
        sqlx::query_as("SELECT state, latest_action FROM threads WHERE id = ?")
            .bind(thread_id)
            .fetch_one(&state.pool)
            .await?;
    eprintln!(
        "processor: completed thread_id={thread_id} state={state_name} latest_action={latest_action}"
    );
    log_thread_state_counts(&state.pool, session.user_id, "processor_complete").await?;
    Ok(true)
}

pub async fn run_classifier(
    state: &AppState,
    session: &UserSession,
    thread_id: i64,
) -> Result<(), AppError> {
    eprintln!("classifier: start thread_id={thread_id}");
    let Some(openrouter_key) = session.openrouter_key.as_deref() else {
        eprintln!("classifier: missing OpenRouter key thread_id={thread_id}");
        create_user_form(
            &state.pool,
            thread_id,
            "AskUser",
            "OpenRouter key required",
            "Add your OpenRouter API key in Settings, then sync again.",
        )
        .await?;
        set_thread_state(
            &state.pool,
            thread_id,
            "NeedsAction",
            "OpenRouter key required",
            None,
        )
        .await?;
        return Ok(());
    };

    log_thread_event(&state.pool, thread_id, "agent", "Classifier started").await?;
    let context = thread_context(&state.pool, thread_id).await?;
    eprintln!(
        "classifier: built context thread_id={thread_id} bytes={}",
        context.len()
    );
    let prompt = format!(
        "Classify this email thread. Return only JSON matching one of: {{\"decision\":\"needs_reply\"}}, {{\"decision\":\"waiting\",\"reason\":\"...\",\"wake_date\":\"YYYY-MM-DD\"}}, {{\"decision\":\"ask_to_resolve\",\"reason\":\"...\"}}. Use needs_reply only when the user owes a reply. Use waiting only when the user has already acted and we need an external response; waiting must include a wake_date. Use ask_to_resolve when no reply is needed and no external response is expected. The classifier cannot directly resolve threads.\n\n{context}"
    );
    eprintln!(
        "classifier: calling OpenRouter thread_id={thread_id} prompt_bytes={}",
        prompt.len()
    );
    let output = call_openrouter_json::<ClassifierOutput>(
        &state.http,
        openrouter_key,
        &prompt,
        "Return exactly one JSON object: {\"decision\":\"needs_reply\"} OR {\"decision\":\"waiting\",\"reason\":\"...\",\"wake_date\":\"YYYY-MM-DD\"} OR {\"decision\":\"ask_to_resolve\",\"reason\":\"...\"}.",
    )
    .await?;

    match output {
        ClassifierOutput::NeedsReply => {
            eprintln!("classifier: decision=needs_reply thread_id={thread_id}");
            set_thread_state(&state.pool, thread_id, "Processing", "Drafting reply", None).await?;
            run_reply_drafter(state, session, thread_id, DraftInstruction::Initial).await?;
        }
        ClassifierOutput::Waiting { reason, wake_date } => {
            eprintln!(
                "classifier: decision=waiting thread_id={thread_id} wake_date={wake_date} reason={reason}"
            );
            set_thread_state(
                &state.pool,
                thread_id,
                "Waiting",
                &format!("Waiting: {reason}"),
                Some(&wake_date),
            )
            .await?;
            log_thread_event(
                &state.pool,
                thread_id,
                "agent",
                &format!("Classifier set Waiting until {wake_date}: {reason}"),
            )
            .await?;
        }
        ClassifierOutput::AskToResolve { reason } => {
            eprintln!("classifier: decision=ask_to_resolve thread_id={thread_id} reason={reason}");
            create_user_form(
                &state.pool,
                thread_id,
                "AskToResolve",
                "Can this be resolved?",
                &reason,
            )
            .await?;
            set_thread_state(
                &state.pool,
                thread_id,
                "NeedsAction",
                "Confirm resolved",
                None,
            )
            .await?;
            log_thread_event(
                &state.pool,
                thread_id,
                "agent",
                "Classifier asked user to resolve",
            )
            .await?;
        }
    }

    eprintln!("classifier: complete thread_id={thread_id}");
    Ok(())
}

async fn run_reply_drafter(
    state: &AppState,
    session: &UserSession,
    thread_id: i64,
    instruction: DraftInstruction<'_>,
) -> Result<(), AppError> {
    eprintln!("reply_drafter: start thread_id={thread_id}");
    let Some(openrouter_key) = session.openrouter_key.as_deref() else {
        eprintln!("reply_drafter: missing OpenRouter key thread_id={thread_id}");
        return Err(AppError("OpenRouter key required".to_owned()));
    };

    log_thread_event(&state.pool, thread_id, "agent", "Reply drafter started").await?;
    let context = thread_context(&state.pool, thread_id).await?;
    eprintln!(
        "reply_drafter: built context thread_id={thread_id} bytes={}",
        context.len()
    );
    let chats = thread_chats(&state.pool, thread_id).await?;
    let prompt = draft_prompt(&context, &chats, &instruction);
    eprintln!(
        "reply_drafter: calling OpenRouter thread_id={thread_id} prompt_bytes={}",
        prompt.len()
    );
    let output = call_openrouter_json::<DraftOutput>(
        &state.http,
        openrouter_key,
        &prompt,
        "Return exactly one JSON object: {\"draft\":\"...\",\"message\":\"short note to user\"}.",
    )
    .await?;
    eprintln!(
        "reply_drafter: parsed draft thread_id={thread_id} bytes={}",
        output.draft.len()
    );

    create_user_form(
        &state.pool,
        thread_id,
        "DraftReply",
        "Review draft reply",
        &output.draft,
    )
    .await?;
    set_thread_state(
        &state.pool,
        thread_id,
        "NeedsAction",
        "Review draft reply",
        None,
    )
    .await?;
    let chat_message = output.message.as_deref().unwrap_or(match instruction {
        DraftInstruction::Initial => "I drafted a reply for review.",
        DraftInstruction::Revision { .. } => "Updated the draft.",
    });
    insert_thread_chat(&state.pool, thread_id, "assistant", chat_message).await?;
    log_thread_event(
        &state.pool,
        thread_id,
        "agent",
        "Reply drafter created draft form",
    )
    .await?;
    eprintln!("reply_drafter: complete thread_id={thread_id}");
    Ok(())
}

pub async fn thread_context(pool: &SqlitePool, thread_id: i64) -> Result<String, sqlx::Error> {
    let thread = sqlx::query_as::<_, ThreadRow>(
        "SELECT id, subject, from_display, state, latest_action, waiting_until, last_message_at, unread FROM threads WHERE id = ?",
    )
    .bind(thread_id)
    .fetch_one(pool)
    .await?;
    let messages = sqlx::query_as::<_, MessageRow>(
        "SELECT sender, body, sent_at FROM messages WHERE thread_id = ? ORDER BY sent_at ASC",
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await?;
    let mut context = format!(
        "Subject: {}\nParticipants: {}\n",
        thread.subject, thread.from_display
    );
    for message in messages {
        write!(
            context,
            "\nMessage from {} at {}:\n{}\n",
            message.sender, message.sent_at, message.body
        )
        .expect("writing to string cannot fail");
    }
    let chats = thread_chats(pool, thread_id).await?;
    if !chats.is_empty() {
        context.push_str("\nDraft assistant chat:\n");
        for chat in chats {
            let role = match chat.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                _ => "Other",
            };
            writeln!(context, "{role}: {}", chat.message).expect("writing to string cannot fail");
        }
    }
    Ok(context)
}

fn draft_prompt(context: &str, chats: &[ChatRow], instruction: &DraftInstruction<'_>) -> String {
    match instruction {
        DraftInstruction::Initial => format!(
            "Draft an email reply for this thread. Return only JSON: {{\"draft\":\"...\",\"message\":\"short note to user\"}}. Message should briefly explain what you drafted.\n\nEmail thread:\n{context}"
        ),
        DraftInstruction::Revision {
            current_draft,
            latest_comment,
        } => {
            let mut prompt = format!(
                "Revise the current draft using the latest user instruction. Return only JSON: {{\"draft\":\"...\",\"message\":\"short note to user\"}}. Message should briefly explain what changed.\n\nEmail thread:\n{context}\n\nCurrent draft:\n{current_draft}\n\nChat history:"
            );
            for chat in chats {
                let role = match chat.role.as_str() {
                    "user" => "User",
                    "assistant" => "Assistant",
                    _ => "Other",
                };
                write!(prompt, "\n{role}: {}", chat.message)
                    .expect("writing to string cannot fail");
            }
            write!(prompt, "\n\nLatest user instruction:\n{latest_comment}")
                .expect("writing to string cannot fail");
            prompt
        }
    }
}
