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
}

export type BridgeStatus = "Stopped" | "Starting" | "Running" | { Error: string };

export type WsStatus = "Stopped" | "Connecting" | "Connected" | "Reconnecting";

export interface BridgeStats {
  uptime_secs: number | null;
  mails_synced: number;
  ws_status: WsStatus;
}

export function statusLabel(status: BridgeStatus): string {
  if (typeof status === "string") return status;
  return `Error: ${status.Error}`;
}

export function isError(status: BridgeStatus): status is { Error: string } {
  return typeof status !== "string";
}
