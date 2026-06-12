import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Config, BridgeStatus, McpPermission } from "../types";

interface Props {
  config: Config | null;
  status: BridgeStatus | null;
  loading: boolean;
  onSave: (config: Config) => Promise<void>;
  onRestart: () => Promise<void>;
}

type Section = "account" | "sync" | "ai";

export function ConfigPanel({ config, status, loading, onSave, onRestart }: Props) {
  const [section, setSection] = useState<Section>("account");
  const [email, setEmail] = useState("");
  const [imapPort, setImapPort] = useState(1143);
  const [smtpPort, setSmtpPort] = useState(1025);
  const [apiUrl, setApiUrl] = useState("https://app.tuta.com");
  const [syncLimit, setSyncLimit] = useState(500);
  const [fetchAll, setFetchAll] = useState(false);
  const [mcpPermission, setMcpPermission] = useState<McpPermission>("disabled");
  const [mcpPort, setMcpPort] = useState(1944);
  const [mcpCopied, setMcpCopied] = useState(false);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    if (config) {
      setEmail(config.email);
      setImapPort(config.imap_port);
      setSmtpPort(config.smtp_port);
      setApiUrl(config.api_url);
      setFetchAll(config.sync_limit === 0);
      setSyncLimit(config.sync_limit === 0 ? 500 : config.sync_limit);
      setMcpPermission(config.mcp_permission ?? "disabled");
      setMcpPort(config.mcp_port ?? 1944);
    }
  }, [config]);

  const isRunning = status === "Running" || status === "Starting";

  const handleSave = async () => {
    await onSave({
      email,
      imap_port: imapPort,
      smtp_port: smtpPort,
      api_url: apiUrl,
      sync_limit: fetchAll ? 0 : syncLimit,
      mcp_permission: mcpPermission,
      mcp_port: mcpPort,
    });
    setSaved(true);
    setTimeout(() => setSaved(false), 2000);
  };

  const handleCopyMcpConfig = async () => {
    try {
      const snippet = await invoke<string>("get_mcp_client_config");
      await navigator.clipboard.writeText(snippet);
      setMcpCopied(true);
      setTimeout(() => setMcpCopied(false), 2000);
    } catch {
      /* clipboard denied, ignore */
    }
  };

  return (
    <div className="panel config-panel">
      <nav className="config-subtabs">
        <button
          className={section === "account" ? "active" : ""}
          onClick={() => setSection("account")}
        >
          Account
        </button>
        <button
          className={section === "sync" ? "active" : ""}
          onClick={() => setSection("sync")}
        >
          Sync
        </button>
        <button
          className={section === "ai" ? "active" : ""}
          onClick={() => setSection("ai")}
        >
          AI access
        </button>
      </nav>

      <div className="config-body">
        {section === "account" && (
          <>
            <div className="form-group">
              <label>Email</label>
              <input
                type="email"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                placeholder="your@tuta.com"
              />
            </div>
            <div className="form-row">
              <div className="form-group">
                <label>IMAP Port</label>
                <input
                  type="number"
                  value={imapPort}
                  onChange={(e) => setImapPort(Number(e.target.value))}
                />
              </div>
              <div className="form-group">
                <label>SMTP Port</label>
                <input
                  type="number"
                  value={smtpPort}
                  onChange={(e) => setSmtpPort(Number(e.target.value))}
                />
              </div>
            </div>
            <div className="form-group">
              <label>API URL</label>
              <input
                type="url"
                value={apiUrl}
                onChange={(e) => setApiUrl(e.target.value)}
              />
            </div>
          </>
        )}

        {section === "sync" && (
          <div className="form-group">
            <label>Offline message bodies</label>
            <small className="field-hint">
              Your whole mailbox is always listed and searchable by subject,
              sender and date. This only sets how many recent message{" "}
              <em>bodies</em> are kept ready offline. Older ones load on demand
              when you open them.
            </small>
            <label className="checkbox-field">
              <input
                type="checkbox"
                checked={fetchAll}
                onChange={(e) => setFetchAll(e.target.checked)}
              />
              <span>Keep every message body offline (full local copy)</span>
            </label>
            {fetchAll ? (
              <small className="field-hint">
                Downloads every body. Slowest option, and uses the most disk on
                large accounts.
              </small>
            ) : (
              <input
                type="number"
                min={1}
                value={syncLimit}
                onChange={(e) => setSyncLimit(Math.max(1, Number(e.target.value)))}
                placeholder="Bodies to keep offline (most recent)"
              />
            )}
          </div>
        )}

        {section === "ai" && (
          <div className="form-group">
            <label>AI access (MCP server)</label>
            <small className="field-hint">
              Lets an LLM client (Claude Desktop / Code) <strong>read</strong>{" "}
              this mailbox over a local MCP server. It is strictly read-only and
              can never send, move or delete mail.
            </small>
            <select
              value={mcpPermission}
              onChange={(e) => setMcpPermission(e.target.value as McpPermission)}
            >
              <option value="disabled">Disabled (off)</option>
              <option value="metadata">
                Metadata only (folders, search, headers)
              </option>
              <option value="full">Full read (includes message bodies)</option>
            </select>
            {mcpPermission === "full" && (
              <small className="field-hint">
                ⚠️ The connected LLM can read full message content. Body text is
                untrusted, so a malicious email could try to mislead the model.
                Only enable with a client you trust.
              </small>
            )}
            {mcpPermission !== "disabled" && (
              <>
                <input
                  type="number"
                  value={mcpPort}
                  onChange={(e) => setMcpPort(Number(e.target.value))}
                  placeholder="MCP port (127.0.0.1)"
                />
                <button type="button" className="secondary" onClick={handleCopyMcpConfig}>
                  {mcpCopied ? "Copied!" : "Copy client config"}
                </button>
                <small className="field-hint">
                  Save first, then paste the copied snippet into your MCP
                  client. The server listens on 127.0.0.1 and requires the
                  bridge password as a bearer token.
                </small>
              </>
            )}
          </div>
        )}
      </div>

      <div className="form-actions">
        {isRunning && (
          <small className="field-hint config-restart-hint">
            Changes apply after a restart.
          </small>
        )}
        <button className="primary" onClick={handleSave} disabled={loading || !email}>
          {saved ? "Saved!" : "Save"}
        </button>
        {isRunning && (
          <button onClick={onRestart} disabled={loading}>
            {loading ? "Restarting…" : "Restart"}
          </button>
        )}
      </div>
    </div>
  );
}
