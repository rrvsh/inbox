DELETE FROM action_logs;
DELETE FROM user_forms;
DELETE FROM messages;
DELETE FROM threads;

ALTER TABLE threads ADD COLUMN gmail_thread_id TEXT;
ALTER TABLE threads ADD COLUMN snippet TEXT;
ALTER TABLE threads ADD COLUMN last_message_at TEXT;
ALTER TABLE messages ADD COLUMN gmail_message_id TEXT;

CREATE UNIQUE INDEX idx_threads_user_gmail_thread ON threads(user_id, gmail_thread_id);
