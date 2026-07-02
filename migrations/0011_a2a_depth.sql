-- v11: add a2a_depth to active_sessions for recursive call-depth tracking.
ALTER TABLE active_sessions ADD COLUMN a2a_depth INTEGER NOT NULL DEFAULT 0;
