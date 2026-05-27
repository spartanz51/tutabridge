export interface Config {
  email: string;
  imap_port: number;
  smtp_port: number;
  api_url: string;
}

export type BridgeStatus = "Stopped" | "Starting" | "Running" | { Error: string };

export interface BridgeStats {
  uptime_secs: number | null;
  mails_synced: number;
}

export function statusLabel(status: BridgeStatus): string {
  if (typeof status === "string") return status;
  return `Error: ${status.Error}`;
}

export function isError(status: BridgeStatus): status is { Error: string } {
  return typeof status !== "string";
}
