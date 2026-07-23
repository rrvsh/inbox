DROP TABLE oauth_states;
ALTER TABLE threads ADD COLUMN user_id INTEGER REFERENCES users(id);
