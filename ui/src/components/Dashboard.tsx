import { useState } from "react";
import type { BridgeStatus, BridgeStats } from "../types";
import { isError } from "../types";

interface Props {
  status: BridgeStatus | null;
  stats: BridgeStats;
  hasSavedSession: boolean;
  loading: boolean;
  onStart: (password?: string) => Promise<void>;
  onStop: () => Promise<void>;
}

function formatUptime(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ${secs % 60}s`;
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return `${h}h ${m}m`;
}

export function Dashboard({ status, stats, hasSavedSession, loading, onStart, onStop }: Props) {
  const [password, setPassword] = useState("");

  if (!status) {
    return <div className="dashboard"><p className="muted">Connecting...</p></div>;
  }

  const isRunning = status === "Running";
  const isStarting = status === "Starting";
  const isStopped = status === "Stopped" || isError(status);
  const needsPassword = isStopped && !hasSavedSession;

  const handleStart = async () => {
    if (needsPassword) {
      await onStart(password);
      setPassword("");
    } else {
      await onStart();
    }
  };

  return (
    <div className="dashboard">
      {isRunning && (
        <div className="status-hero running">
          <div className="hero-indicator">
            <span className="pulse-ring" />
            <span className="pulse-dot" />
          </div>
          <div className="hero-text">
            <strong>Bridge is running</strong>
            {stats.uptime_secs != null && (
              <span className="hero-uptime">Up {formatUptime(stats.uptime_secs)}</span>
            )}
          </div>
          <button className="hero-action" onClick={onStop} disabled={loading}>
            {loading ? "Stopping..." : "Stop"}
          </button>
        </div>
      )}

      {isStarting && (
        <div className="status-hero starting">
          <div className="hero-indicator">
            <span className="pulse-ring orange" />
            <span className="pulse-dot orange" />
          </div>
          <div className="hero-text">
            <strong>Connecting...</strong>
          </div>
        </div>
      )}

      {isStopped && (
        <div className="status-hero stopped">
          <div className="hero-indicator">
            <span className="static-dot" style={{ background: isError(status) ? "var(--red)" : "var(--gray)" }} />
          </div>
          <div className="hero-text">
            <strong>{isError(status) ? "Connection failed" : "Bridge is stopped"}</strong>
          </div>
        </div>
      )}

      {isError(status) && (
        <p className="error-text">{status.Error}</p>
      )}

      {isStopped && (
        <div className="start-section">
          {needsPassword && (
            <div className="form-group">
              <label>Tuta Password</label>
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder="Enter your Tuta password"
                onKeyDown={(e) => e.key === "Enter" && password && handleStart()}
              />
            </div>
          )}
          <button
            className="primary start-btn"
            onClick={handleStart}
            disabled={loading || (needsPassword && !password)}
          >
            {loading ? "Connecting..." : "Start Bridge"}
          </button>
        </div>
      )}

      <div className="stats-grid">
        <div className="stat-card">
          <span className="stat-value">{stats.mails_synced}</span>
          <span className="stat-label">Emails synced</span>
        </div>
        <div className="stat-card">
          <span className="stat-value">
            {stats.uptime_secs != null ? formatUptime(stats.uptime_secs) : "--"}
          </span>
          <span className="stat-label">Uptime</span>
        </div>
      </div>
    </div>
  );
}
