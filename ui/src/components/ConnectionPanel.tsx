import { useState } from "react";
import type { Config, BridgeStatus } from "../types";

interface Props {
  config: Config | null;
  status: BridgeStatus | null;
  bridgePassword: string | null;
  onRegeneratePassword: () => Promise<string>;
}

export function ConnectionPanel({ config, status, bridgePassword, onRegeneratePassword }: Props) {
  const [showPassword, setShowPassword] = useState(false);
  const [copied, setCopied] = useState(false);

  const isRunning = status === "Running" || status === "Starting";

  const copyToClipboard = (text: string) => {
    navigator.clipboard.writeText(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  if (!config) {
    return <div className="connection-panel"><p className="muted">Loading...</p></div>;
  }

  return (
    <div className="connection-panel">
      <p className="panel-subtitle">Configure your mail client with these settings</p>

      <div className="settings-row">
        <div className="settings-card">
          <div className="settings-section">
            <h3>Incoming (IMAP)</h3>
            <div className="settings-grid">
              <span className="setting-key">Server</span>
              <code className="setting-val">127.0.0.1</code>
              <span className="setting-key">Port</span>
              <code className="setting-val">{config.imap_port}</code>
              <span className="setting-key">Security</span>
              <code className="setting-val">SSL/TLS</code>
            </div>
          </div>
        </div>

        <div className="settings-card">
          <div className="settings-section">
            <h3>Outgoing (SMTP)</h3>
            <div className="settings-grid">
              <span className="setting-key">Server</span>
              <code className="setting-val">127.0.0.1</code>
              <span className="setting-key">Port</span>
              <code className="setting-val">{config.smtp_port}</code>
              <span className="setting-key">Security</span>
              <code className="setting-val">SSL/TLS</code>
            </div>
          </div>
        </div>
      </div>

      <div className="settings-card">
        <div className="settings-section">
          <h3>Authentication</h3>
          <div className="settings-grid">
            <span className="setting-key">Username</span>
            <code className="setting-val copyable" onClick={() => copyToClipboard(config.email)}>
              {config.email || "(not set)"}
            </code>
            <span className="setting-key">Password</span>
            <div className="password-field-inline">
              {bridgePassword ? (
                <>
                  <code className="setting-val password-mono">
                    {showPassword ? bridgePassword : "•".repeat(23)}
                  </code>
                  <button
                    className="inline-btn"
                    onClick={() => setShowPassword(!showPassword)}
                  >
                    {showPassword ? "Hide" : "Show"}
                  </button>
                  <button
                    className="inline-btn"
                    onClick={() => copyToClipboard(bridgePassword)}
                  >
                    {copied ? "Copied!" : "Copy"}
                  </button>
                </>
              ) : (
                <span className="muted">Start the bridge to generate</span>
              )}
            </div>
          </div>
        </div>
      </div>

      {bridgePassword && (
        <button
          className="small regen-btn"
          onClick={onRegeneratePassword}
          disabled={isRunning}
          title={isRunning ? "Stop the bridge first" : "Generate a new password"}
        >
          Regenerate password
        </button>
      )}

      <div className="settings-note">
        <strong>Note:</strong> Accept the self-signed certificate when your mail client prompts.
      </div>
    </div>
  );
}
