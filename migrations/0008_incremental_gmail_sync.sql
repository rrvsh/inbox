ALTER TABLE users ADD COLUMN token_expires_at INTEGER;
ALTER TABLE users ADD COLUMN gmail_history_id TEXT;
ALTER TABLE users ADD COLUMN gmail_reauth_required INTEGER NOT NULL DEFAULT 0;
