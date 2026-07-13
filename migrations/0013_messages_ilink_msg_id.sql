-- v13: add ilink_msg_id to messages for exact quote-reply routing.
--
-- Stores the client-set `message_id` the Hub sends to iLink on outbound
-- assistant replies. iLink preserves this id and echoes it back as
-- `ref_msg.message_item.msg_id` when a user quote-replies, so the Hub can
-- route the follow-up to the exact backend/session that produced the quoted
-- message — replacing the ±10s time-window fallback (L1) for new rows.
--
-- Nullable: pre-v13 rows and user-side messages have no ilink_msg_id; they
-- continue to resolve via the L1/content/footer fallbacks.
ALTER TABLE messages ADD COLUMN ilink_msg_id INTEGER;

-- Exact-lookup index. Only outbound assistant rows populate the column.
CREATE INDEX IF NOT EXISTS idx_messages_ilink_msg_id ON messages (ilink_msg_id);
