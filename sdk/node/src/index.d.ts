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
  /**
   * Send a partial response chunk to the WeChat user immediately.
   * Writes `ILINK_PARTIAL:<json>` to stdout and flushes.
   * The bridge forwards the text in real-time without waiting for the process to exit.
   */
  sendPartial(text: string): void;
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

// ---------------------------------------------------------------------------
// AgentlyMailClient
// ---------------------------------------------------------------------------

export class AgentlyMailError extends Error {
  exitCode: number;
  serverMessage?: string;
}

export interface ListOptions {
  dir?: 'inbox' | 'sent' | 'trash' | 'spam';
  limit?: number;
  cursor?: string;
  after?: string;
  before?: string;
  hasAttachments?: boolean;
  isUnread?: boolean;
}

export interface SearchOptions extends ListOptions {
  searchIn?: 'SEARCH_IN_ALL' | 'SEARCH_IN_SUBJECT' | 'SEARCH_IN_CONTENT';
  from?: string;
  to?: string;
}

export interface SendOptions {
  cc?: string | string[];
  bcc?: string | string[];
  bodyFormat?: 'plain' | 'html';
  attachments?: string[];
}

export interface ReplyOptions {
  replyAll?: boolean;
  cc?: string | string[];
  bcc?: string | string[];
  bodyFormat?: 'plain' | 'html';
  attachments?: string[];
}

export interface ForwardOptions {
  cc?: string | string[];
  bcc?: string | string[];
  bodyFormat?: 'plain' | 'html';
  includeAttachments?: boolean;
  attachments?: string[];
}

export interface PollOptions {
  limit?: number;
}

export interface PollerController {
  stop(): void;
}

export type PollHandler = (
  msgSummary: object,
  client: AgentlyMailClient,
) => Promise<void>;

export class AgentlyMailClient {
  /** List messages with optional filters. */
  list(options?: ListOptions): { messages: object[]; pagination: object };

  /** List only unread messages (convenience method). */
  listUnread(limit?: number): object[];

  /** Read a single message in full (body + attachments). */
  read(messageId: string): object;

  /** Search messages by keyword. */
  search(
    query: string,
    options?: SearchOptions,
  ): { messages: object[]; pagination: object };

  /** Get current user info and alias list. */
  me(): object;

  /** Send a new email (two-phase confirmation handled automatically). */
  send(
    to: string | string[],
    subject: string,
    body: string,
    options?: SendOptions,
  ): object;

  /** Reply to a message (two-phase confirmation handled automatically). */
  reply(messageId: string, body: string, options?: ReplyOptions): object;

  /** Forward a message (two-phase confirmation handled automatically). */
  forward(
    messageId: string,
    to: string | string[],
    body?: string,
    options?: ForwardOptions,
  ): object;

  /** Move a message to trash (two-phase confirmation handled automatically). */
  trash(messageId: string): object;

  /**
   * Poll for unread messages at a fixed interval.
   * Handler is called once per unread message; errors are logged and polling continues.
   */
  poll(
    intervalMs: number,
    handler: PollHandler,
    options?: PollOptions,
  ): PollerController;
}
