-- Normalize `context_token_map.peer_user_id` to the canonical `peer:` / `group:` form.
--
-- A pre-v6 code path stored `peer_user_id` as the bare WeChat peer ID
-- (e.g. `o9cq80_ZyXuz1vAtG-TMbQjwQPW8@im.wechat`). The current
-- `find_or_create_vctx` writes a `conv_key` with a `peer:` / `group:` prefix
-- (e.g. `peer:o9cq80_...`) and queries by that prefixed value. On a pre-v6
-- database the query never matched the existing row, so every new message
-- minted a fresh vctx — orphaning the previous conversation and all its
-- backend sessions.
--
-- This migration prepends `peer:` to any non-empty `peer_user_id` that does
-- not already start with `peer:` or `group:`. Empty values are intentionally
-- left alone (they represent messages where neither `peer_user_id` nor
-- `group_id` was known, and `find_or_create_vctx` already handles those by
-- minting a fresh vctx per call). The UPDATE is idempotent — re-running it
-- matches no rows because every non-empty value now starts with `peer:` or
-- `group:`.

UPDATE context_token_map
SET peer_user_id = 'peer:' || peer_user_id
WHERE peer_user_id != ''
  AND peer_user_id NOT LIKE 'peer:%'
  AND peer_user_id NOT LIKE 'group:%';