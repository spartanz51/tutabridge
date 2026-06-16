import { useState } from "react";
import { useBridge } from "./hooks/useBridge";
import { Dashboard } from "./components/Dashboard";
import { ConnectionPanel } from "./components/ConnectionPanel";
import { ConfigPanel } from "./components/ConfigPanel";
import { BackupPanel } from "./components/BackupPanel";
import { statusLabel, isError } from "./types";
import logo from "./assets/logo.png";
import "./App.css";

type Tab = "dashboard" | "connection" | "config" | "backup";

function App() {
  const [tab, setTab] = useState<Tab>("dashboard");
  const bridge = useBridge();

  const status = bridge.status;
  const isRunning = status === "Running";
  const isStarting = status === "Starting";
  const statusColor = isRunning
    ? "var(--green)"
    : isStarting
      ? "var(--orange)"
      : status && isError(status)
        ? "var(--red)"
        : "var(--gray)";

  return (
    <div className="app">
      <header className="app-header">
        <div className="header-left">
          <img src={logo} alt="" className="app-logo" width={24} height={24} />
          <h1>TutaBridge</h1>
          <div className="header-status">
            <span className="status-dot" style={{ background: statusColor }} />
            <span className="header-status-text">
              {status ? statusLabel(status) : "Loading..."}
            </span>
          </div>
        </div>
        <nav className="tabs">
          <button
            className={tab === "dashboard" ? "active" : ""}
            onClick={() => setTab("dashboard")}
          >
            Dashboard
          </button>
          <button
            className={tab === "connection" ? "active" : ""}
            onClick={() => setTab("connection")}
          >
            Connection
          </button>
          <button
            className={tab === "config" ? "active" : ""}
            onClick={() => setTab("config")}
          >
            Config
          </button>
          <button
            className={tab === "backup" ? "active" : ""}
            onClick={() => setTab("backup")}
          >
            Backup
          </button>
        </nav>
      </header>
      <main className="app-content">
        {tab === "dashboard" && (
          <Dashboard
            status={bridge.status}
            stats={bridge.stats}
            config={bridge.config}
            hasSavedSession={bridge.hasSavedSession}
            loading={bridge.loading}
            logs={bridge.logs}
            needsTotp={bridge.needsTotp}
            onStart={bridge.startBridge}
            onSubmitTotp={bridge.submitTotp}
            onStop={bridge.stopBridge}
            onClearLogs={bridge.clearLogs}
          />
        )}
        {tab === "connection" && (
          <ConnectionPanel
            config={bridge.config}
            status={bridge.status}
            bridgePassword={bridge.bridgePassword}
            onRegeneratePassword={bridge.regenerateBridgePassword}
          />
        )}
        {tab === "config" && (
          <ConfigPanel
            config={bridge.config}
            status={bridge.status}
            loading={bridge.loading}
            onSave={bridge.saveConfig}
            onRestart={bridge.restartBridge}
          />
        )}
        {tab === "backup" && (
          <BackupPanel
            isRunning={isRunning}
            busy={bridge.backupBusy}
            progress={bridge.backupProgress}
            result={bridge.backupResult}
            error={bridge.backupError}
            onBackup={bridge.startBackup}
          />
        )}
      </main>
    </div>
  );
}

export default App;
