-- v14: persist last AgentProc usage stats on backend sessions.
--
-- Bridge reports optional `usage` (input_tokens / output_tokens / …) via
-- `ilink_hub_ext.usage` on sendmessage; Hub stores the most recent value for
-- the active named session so operators can inspect token/cost stats.

ALTER TABLE backend_sessions_v2 ADD COLUMN last_usage_json TEXT;
