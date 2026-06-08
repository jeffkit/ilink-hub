export interface ProfileContext {
  /** User message text (ILINK_MESSAGE) */
  message: string;
  /** Hub-persisted backend session UUID (ILINK_SESSION_ID) */
  sessionId: string;
  /** Human-readable session name (ILINK_SESSION_NAME) */
  sessionName: string;
  /** Sender user ID (ILINK_FROM_USER) */
  fromUser: string;
  /** Hub context token (ILINK_CONTEXT_TOKEN) */
  contextToken: string;
}

export interface ProfileResult {
  /** Reply text to send back to the WeChat user */
  response: string;
  /** New backend session ID to persist (optional) */
  sessionId?: string;
}

export type ProfileHandler = (
  ctx: ProfileContext
) => Promise<ProfileResult | string>;

/**
 * Run a profile handler following the P0 exec protocol.
 * Reads ILINK_* env vars, calls `handler`, writes P0 stdout, then exits.
 */
export function createProfile(handler: ProfileHandler): void;

export interface HistoryEntry {
  role: 'user' | 'assistant' | string;
  content: string;
  ts: string;
}

/**
 * Load conversation history for a session from its JSONL file.
 * Returns an empty array if the file does not exist.
 */
export function loadHistory(sessionId: string, sessionDir?: string): HistoryEntry[];

/**
 * Append entries to a session's JSONL history file.
 * Creates the file (and parent directory) if needed.
 */
export function appendHistory(
  sessionId: string,
  entries: Omit<HistoryEntry, 'ts'>[],
  sessionDir?: string
): void;

/** Resolved path for a session JSONL file. */
export function sessionFilePath(sessionId: string, sessionDir?: string): string;
