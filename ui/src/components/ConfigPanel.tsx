import { useState, useEffect } from "react";
import type { Config, BridgeStatus } from "../types";

interface Props {
  config: Config | null;
  status: BridgeStatus | null;
  onSave: (config: Config) => Promise<void>;
}

export function ConfigPanel({ config, status, onSave }: Props) {
  const [email, setEmail] = useState("");
  const [imapPort, setImapPort] = useState(1143);
  const [smtpPort, setSmtpPort] = useState(1025);
  const [apiUrl, setApiUrl] = useState("https://app.tuta.com");
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    if (config) {
      setEmail(config.email);
      setImapPort(config.imap_port);
      setSmtpPort(config.smtp_port);
      setApiUrl(config.api_url);
    }
  }, [config]);

  const isRunning = status === "Running" || status === "Starting";

  const handleSave = async () => {
    await onSave({
      email,
      imap_port: imapPort,
      smtp_port: smtpPort,
      api_url: apiUrl,
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
          disabled={isRunning}
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
            disabled={isRunning}
          />
        </div>
        <div className="form-group">
          <label>SMTP Port</label>
          <input
            type="number"
            value={smtpPort}
            onChange={(e) => setSmtpPort(Number(e.target.value))}
            disabled={isRunning}
          />
        </div>
      </div>
      <div className="form-group">
        <label>API URL</label>
        <input
          type="url"
          value={apiUrl}
          onChange={(e) => setApiUrl(e.target.value)}
          disabled={isRunning}
        />
      </div>
      <button onClick={handleSave} disabled={isRunning || !email}>
        {saved ? "Saved!" : "Save"}
      </button>
    </div>
  );
}
