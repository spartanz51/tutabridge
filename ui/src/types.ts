/** Read-only MCP server access tier. `disabled` = server off. */
export type McpPermission = "disabled" | "metadata" | "full";

export interface Config {
  email: string;
  imap_port: number;
  smtp_port: number;
  api_url: string;
  /** Max mails synced per folder; 0 = fetch all. */
  sync_limit: number;
  /** Read-only MCP server permission tier. */
  mcp_permission: McpPermission;
  /** Port the read-only MCP HTTP server listens on (127.0.0.1). */
  mcp_port: number;
  /** The app looks for new versions on GitHub and installs them. */
  auto_update: boolean;
}

/** A version newer than the running one, from `check_update`. */
export interface UpdateInfo {
  version: string;
  current: string;
  notes: string | null;
}

export type BridgeStatus = "Stopped" | "Starting" | "Running" | { Error: string };

export type WsStatus = "Stopped" | "Connecting" | "Connected" | "Reconnecting";

export interface BridgeStats {
  uptime_secs: number | null;
  /** When the running bridge started, ms since the epoch. */
  started_at_ms: number | null;
  mails_synced: number;
  ws_status: WsStatus;
  /** Mail apps signed in over IMAP. */
  clients: ClientInfo[];
}

/** A mail app signed in over IMAP (one per connection). */
export interface ClientInfo {
  id: number;
  peer: string;
  /** From the IMAP ID command, when the app sends it. */
  name: string | null;
  version: string | null;
  os: string | null;
  connected_at_ms: number;
  folder: string | null;
}

/** What the bridge just did. Content-free: folders and counts only. */
export type ActivityEvent =
  | { kind: "synced"; client: number; folder: string; messages: number }
  | { kind: "body_downloaded"; client: number }
  | { kind: "marked_read"; client: number; count: number; read: boolean }
  | { kind: "moved"; client: number; count: number; folder: string }
  | { kind: "deleted"; client: number; count: number }
  | { kind: "new_mail"; folder: string }
  | { kind: "sent" };

export function statusLabel(status: BridgeStatus): string {
  // The header pill only names the state; the Dashboard shows the reason.
  return typeof status === "string" ? status : "Error";
}

export function isError(status: BridgeStatus): status is { Error: string } {
  return typeof status !== "string";
}
