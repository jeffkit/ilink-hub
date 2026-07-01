export { AgentlyMailClient, AgentlyMailError } from './agently-mail';

export interface ProfileConfig {
  command: string;
  args?: string[];
  trigger?: string;
  description?: string;
}

export interface ProfilesConfig {
  default: string;
  profiles: Record<string, ProfileConfig>;
}

export interface DispatchResult {
  response: string;
  profileName: string;
}

export class ProfileDispatcher {
  config: ProfilesConfig;
  configPath: string;
  configDir: string;

  constructor(configPath: string);

  /** List configured profile names. */
  profileNames(): string[];

  /** Resolve profile from email subject. */
  resolveProfile(subject: string): {
    profileName: string;
    profileConfig: ProfileConfig;
    cleanSubject: string;
  };

  /** Dispatch a full message to the resolved Profile and return the response. */
  dispatch(fullMsg: object, dryRun?: boolean): DispatchResult;
}

export interface EmailBridgeOptions {
  /** Path to email-profiles.yaml (default: ./email-profiles.yaml) */
  profilesConfig?: string;
  /** Poll interval in milliseconds (default: 300_000) */
  pollIntervalMs?: number;
  /** Skip actual email replies (default: false) */
  dryRun?: boolean;
  /** Max unread emails per poll cycle (default: 20) */
  limit?: number;
}

export interface BridgeController {
  stop(): void;
}

/**
 * Start the email bridge daemon.
 * Polls the mailbox, routes each unread email to a Profile, and replies.
 */
export function createEmailBridge(options?: EmailBridgeOptions): BridgeController;
