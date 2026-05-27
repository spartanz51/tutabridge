import { useState, useEffect } from "react";
import type { Config, BridgeStatus } from "../types";

interface Props {
  config: Config | null;
  status: BridgeStatus | null;
  loading: boolean;
  onSave: (config: Config) => Promise<void>;
  onRestart: () => Promise<void>;
}

export function ConfigPanel({ config, status, loading, onSave, onRestart }: Props) {
  const [email, setEmail] = useState("");
  const [imapPort, setImapPort] = useState(1143);
  const [smtpPort, setSmtpPort] = useState(1025);
  const [apiUrl, setApiUrl] = useState("https://app.tuta.com");
  const [syncLimit, setSyncLimit] = useState(500);
  const [fetchAll, setFetchAll] = useState(false);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    if (config) {
      setEmail(config.email);
      setImapPort(config.imap_port);
      setSmtpPort(config.smtp_port);
      setApiUrl(config.api_url);
      setFetchAll(config.sync_limit === 0);
      setSyncLimit(config.sync_limit === 0 ? 500 : config.sync_limit);
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
    });
    setSaved(true);
    setTimeout(() => setSaved(false), 2000);
  };

  return (
    <div className="panel">
      <h2>Configuration</h2>
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
      <div className="form-group">
        <label>Mail to sync</label>
        <label className="checkbox-field">
          <input
            type="checkbox"
            checked={fetchAll}
            onChange={(e) => setFetchAll(e.target.checked)}
          />
          <span>Fetch all mail (entire account, kept locally)</span>
        </label>
        {fetchAll ? (
          <small className="field-hint">
            Downloads every mail from the start — can be slow on large accounts.
          </small>
        ) : (
          <input
            type="number"
            min={1}
            value={syncLimit}
            onChange={(e) => setSyncLimit(Math.max(1, Number(e.target.value)))}
            placeholder="Max mails per folder"
          />
        )}
      </div>
      {isRunning && (
        <small className="field-hint">Changes apply after a restart.</small>
      )}
      <div className="form-actions">
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
