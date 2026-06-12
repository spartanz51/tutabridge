import { useState, useEffect, useRef } from "react";
import type { BridgeStatus, BridgeStats } from "../types";
import { isError } from "../types";

interface Props {
  status: BridgeStatus | null;
  stats: BridgeStats;
  hasSavedSession: boolean;
  loading: boolean;
  logs: string[];
  onStart: (password?: string) => Promise<void>;
  onStop: () => Promise<void>;
  onClearLogs: () => void;
}

function formatUptime(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ${secs % 60}s`;
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return `${h}h ${m}m`;
}

export function Dashboard({
  status,
  stats,
  hasSavedSession,
  loading,
  logs,
  onStart,
  onStop,
  onClearLogs,
}: Props) {
  const [password, setPassword] = useState("");
  const logEndRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    logEndRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [logs]);

  if (!status) {
    return (
      <div className="dashboard">
        <p className="muted">Connecting...</p>
      </div>
    );
  }

  const isRunning = status === "Running";
  const isStarting = status === "Starting";
  const errored = isError(status);
  const isStopped = status === "Stopped" || errored;
  const needsPassword = isStopped && !hasSavedSession;
  const wsConnected = stats.ws_status === "Connected";

  // LED colour reflects real health: green only when running and realtime is
  // actually connected, orange while connecting or reconnecting, grey when off.
  const led = isRunning ? (wsConnected ? "running" : "starting") : isStarting ? "starting" : "stopped";
  const accent = isRunning ? "running" : isStarting ? "starting" : "stopped";

  const title = isRunning
    ? "Bridge running"
    : isStarting
      ? "Connecting…"
      : errored
        ? "Connection failed"
        : "Bridge stopped";

  // Subtitle only carries information the rest of the screen doesn't already
  // show: nothing when everything is healthy.
  const subtitle = isRunning
    ? wsConnected
      ? ""
      : "Realtime reconnecting…"
    : isStarting
      ? "Signing in and syncing your mailbox"
      : errored
        ? "See the activity log below"
        : "Start the bridge to connect your mail client";

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
      <div className={`status-bar ${accent}`}>
        <span className={`status-led ${led}`} />
        <div className="status-bar-text">
          <strong>{title}</strong>
          {subtitle && <span className="status-bar-sub">{subtitle}</span>}
        </div>
        {(isRunning || isStarting) && (
          <button className="status-action" onClick={onStop} disabled={loading}>
            {loading ? "Stopping…" : "Stop"}
          </button>
        )}
      </div>

      {errored && <p className="error-text">{status.Error}</p>}

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
            {loading ? "Connecting…" : "Start Bridge"}
          </button>
        </div>
      )}

      <div className="stats-grid">
        <div className="stat-card">
          <span className="stat-value">{stats.mails_synced.toLocaleString()}</span>
          <span className="stat-label">Emails synced</span>
        </div>
        <div className="stat-card">
          <span className="stat-value">
            {stats.uptime_secs != null ? formatUptime(stats.uptime_secs) : "—"}
          </span>
          <span className="stat-label">Uptime</span>
        </div>
      </div>

      <div className="dash-logs">
        <div className="dash-logs-header">
          <span className="dash-logs-title">Activity</span>
          {logs.length > 0 && (
            <button className="inline-btn" onClick={onClearLogs}>
              Clear
            </button>
          )}
        </div>
        <div className="logs-container">
          {logs.length === 0 ? (
            <span className="logs-empty">Waiting for activity…</span>
          ) : (
            logs.map((line, i) => (
              <div key={i} className="log-line">
                {line}
              </div>
            ))
          )}
          <div ref={logEndRef} />
        </div>
      </div>
    </div>
  );
}
