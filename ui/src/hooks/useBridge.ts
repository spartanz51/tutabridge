import { useState, useEffect, useCallback, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import type { ActivityEvent, Config, BridgeStatus, BridgeStats, UpdateInfo } from "../types";
import { pushFeed, type FeedItem } from "../activity";

const MAX_LOG_LINES = 500;

interface SessionStatus {
  available: boolean;
  warning: string | null;
}

export interface BackupStats {
  folders: number;
  mails_written: number;
  from_cache: number;
  from_server: number;
  skipped: number;
  bytes: number;
  errors: string[];
}

export interface BackupProgress {
  folder: string;
  done: number;
  total: number;
}

export function useBridge() {
  const [config, setConfig] = useState<Config | null>(null);
  const [status, setStatus] = useState<BridgeStatus | null>(null);
  const [stats, setStats] = useState<BridgeStats>({
    uptime_secs: null,
    started_at_ms: null,
    mails_synced: 0,
    ws_status: "Stopped",
    clients: [],
  });
  // Live activity: the feed survives tab switches; the Live view subscribes
  // to each event for its animation.
  const [feed, setFeed] = useState<FeedItem[]>([]);
  const activityListeners = useRef(new Set<(event: ActivityEvent) => void>());
  const subscribeActivity = useCallback((listener: (event: ActivityEvent) => void) => {
    activityListeners.current.add(listener);
    return () => {
      activityListeners.current.delete(listener);
    };
  }, []);
  const clientsRef = useRef(stats.clients);
  useEffect(() => {
    clientsRef.current = stats.clients;
  }, [stats.clients]);
  // null means unchecked or unreadable, not a confirmed missing session.
  const [hasSavedSession, setHasSavedSession] = useState<boolean | null>(null);
  const [sessionReadError, setSessionReadError] = useState<string | null>(null);
  const [sessionWarning, setSessionWarning] = useState<string | null>(null);
  const sessionReadEpoch = useRef(0);
  const wasRunning = useRef(false);
  const [bridgePassword, setBridgePassword] = useState<string | null>(null);
  const [logs, setLogs] = useState<string[]>([]);
  const [loading, setLoading] = useState(false);
  const [needsTotp, setNeedsTotp] = useState(false);

  // Backup state lives here (not in BackupPanel) so it survives tab
  // switches — the panel is conditionally rendered and would otherwise
  // unmount mid-export, dropping its progress + the event listener while
  // the Rust task keeps running.
  const [backupBusy, setBackupBusy] = useState(false);
  const [backupProgress, setBackupProgress] = useState<BackupProgress | null>(null);
  const [backupResult, setBackupResult] = useState<BackupStats | null>(null);
  const [backupError, setBackupError] = useState<string | null>(null);

  // In-app updates: the running version, and the version installed behind
  // it that waits for a restart (from the background loop or a manual install).
  const [appVersion, setAppVersion] = useState("");
  const [updateReady, setUpdateReady] = useState<string | null>(null);
  // A version the background loop found but did not install (it would need
  // more rights): the banner offers the install instead.
  const [updateAvailable, setUpdateAvailable] = useState<string | null>(null);

  const readSavedSession = useCallback(async () => {
    const epoch = ++sessionReadEpoch.current;
    try {
      const session = await invoke<SessionStatus>("has_saved_session");
      if (epoch !== sessionReadEpoch.current) return null;
      setHasSavedSession(session.available);
      setSessionWarning(session.warning);
      setSessionReadError(null);
      return session.available;
    } catch (e) {
      if (epoch !== sessionReadEpoch.current) return null;
      setHasSavedSession(null);
      setSessionWarning(null);
      setSessionReadError(String(e));
      return null;
    }
  }, []);

  const acceptStatus = useCallback((next: BridgeStatus) => {
    setStatus(next);
    if (next === "Running" && !wasRunning.current) {
      // A successful login can still be memory-only. Refresh the storage
      // status so its warning survives successful startup and status events.
      setSessionReadError(null);
      void readSavedSession();
    }
    wasRunning.current = next === "Running";
  }, [readSavedSession]);

  const refresh = useCallback(() => {
    invoke<BridgeStatus>("get_status").then(acceptStatus);
    invoke<BridgeStats>("get_stats").then(setStats);
  }, [acceptStatus]);

  useEffect(() => {
    invoke<Config>("get_config").then(setConfig);
    // Strict storage errors block login; an unavailable OS keychain offers
    // a manual login with a visible warning that it cannot be persisted.
    void readSavedSession();
    invoke<string | null>("get_bridge_password").then(setBridgePassword);
    invoke<string>("get_app_version").then(setAppVersion);
    refresh();

    // The bridge pushes `bridge://stats` and `bridge://status` whenever
    // anything changes (mail count, ws state, start/stop). No setInterval.
    const unlistenStats = listen<BridgeStats>("bridge://stats", (e) => setStats(e.payload));
    const unlistenStatus = listen<BridgeStatus>("bridge://status", (e) => acceptStatus(e.payload));
    // The login (still in progress) needs a 2FA code: show the field.
    const unlistenTotp = listen("bridge://need-totp", () => setNeedsTotp(true));
    const unlistenUpdate = listen<{ version: string }>("bridge://update-ready", (e) => {
      setUpdateAvailable(null);
      setUpdateReady(e.payload.version);
    });
    unlistenUpdate.then(() => invoke<string | null>("pending_update").then((version) => {
      if (version) setUpdateReady(version);
    }));
    const unlistenAvailable = listen<{ version: string }>("bridge://update-available", (e) =>
      setUpdateAvailable(e.payload.version),
    );
    return () => {
      unlistenStats.then((fn) => fn());
      unlistenStatus.then((fn) => fn());
      unlistenTotp.then((fn) => fn());
      unlistenUpdate.then((fn) => fn());
      unlistenAvailable.then((fn) => fn());
    };
  }, [refresh, readSavedSession, acceptStatus]);

  useEffect(() => {
    let seq = 0;
    const listeners = activityListeners.current;
    const unlisten = listen<ActivityEvent>("bridge://activity", (e) => {
      seq += 1;
      const id = seq;
      setFeed((prev) => pushFeed(prev, e.payload, clientsRef.current, Date.now(), id));
      listeners.forEach((listener) => listener(e.payload));
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  useEffect(() => {
    const unlisten = listen<string>("bridge://log", (event) => {
      setLogs((prev) => {
        const next = [...prev, event.payload];
        return next.length > MAX_LOG_LINES ? next.slice(-MAX_LOG_LINES) : next;
      });
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  // Always-on backup progress listener — independent of which tab is
  // mounted, so progress keeps flowing even when the Backup tab is hidden.
  useEffect(() => {
    const unlisten = listen<BackupProgress & { finished: boolean }>(
      "bridge://backup-progress",
      (e) => {
        if (e.payload.finished) {
          setBackupProgress(null);
        } else {
          setBackupProgress({
            folder: e.payload.folder,
            done: e.payload.done,
            total: e.payload.total,
          });
        }
      },
    );
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  const saveConfig = useCallback(async (cfg: Config) => {
    const saved = await invoke<Config>("save_config", { config: cfg });
    setConfig(saved);
    await readSavedSession();
  }, [readSavedSession]);

  // One login. If the account has 2FA, the backend emits `bridge://need-totp`
  // mid-login and waits for `submitTotp`; this promise stays pending until then.
  const startBridge = useCallback(
    async (password?: string, email?: string) => {
      setLoading(true);
      setNeedsTotp(false);
      try {
        // A previous pinentry failure may have been resolved, or the user
        // may have removed the entry. Recheck before deciding to sign in.
        const saved = await readSavedSession();
        if (saved === null || (!saved && !password)) return;
        await invoke("start_bridge", {
          password: password || null,
          email: email || null,
        });
        await readSavedSession();
        setNeedsTotp(false);
        refresh();
        invoke<string | null>("get_bridge_password").then(setBridgePassword);
      } catch (e) {
        setNeedsTotp(false);
        setStatus({ Error: String(e) });
      } finally {
        setLoading(false);
      }
    },
    [refresh, readSavedSession],
  );

  // Deliver the 2FA code to the login that's currently waiting for it.
  const submitTotp = useCallback(async (code: string) => {
    try {
      await invoke("submit_totp", { code });
    } catch (e) {
      setStatus({ Error: String(e) });
    }
  }, []);

  const stopBridge = useCallback(async () => {
    setLoading(true);
    try {
      await invoke("stop_bridge");
      refresh();
    } catch (e) {
      setStatus({ Error: String(e) });
    } finally {
      setLoading(false);
    }
  }, [refresh]);

  const restartBridge = useCallback(async () => {
    setLoading(true);
    try {
      await invoke("stop_bridge");
      await invoke("start_bridge", { password: null });
      await readSavedSession();
      refresh();
    } catch (e) {
      setStatus({ Error: String(e) });
    } finally {
      setLoading(false);
    }
  }, [refresh, readSavedSession]);


  const checkUpdate = useCallback(() => invoke<UpdateInfo | null>("check_update"), []);

  const installUpdate = useCallback(async () => {
    const installed = await invoke<string | null>("install_update");
    if (installed) {
      setUpdateAvailable(null);
      setUpdateReady(installed);
    }
    return installed;
  }, []);

  const restartApp = useCallback(() => invoke("restart_app"), []);

  const regenerateBridgePassword = useCallback(async () => {
    const newPassword = await invoke<string>("regenerate_bridge_password");
    setBridgePassword(newPassword);
    return newPassword;
  }, []);

  const startBackup = useCallback(async () => {
    if (backupBusy) return;
    setBackupError(null);
    setBackupResult(null);
    let dir: string | null = null;
    try {
      const picked = await open({ directory: true, title: "Choose a backup folder" });
      dir = typeof picked === "string" ? picked : null;
    } catch (e) {
      setBackupError(String(e));
      return;
    }
    if (!dir) return;

    setBackupBusy(true);
    setBackupProgress(null);
    try {
      const stats = await invoke<BackupStats>("export_mails", { outputDir: dir });
      setBackupResult(stats);
    } catch (e) {
      setBackupError(String(e));
    } finally {
      setBackupBusy(false);
      setBackupProgress(null);
    }
  }, [backupBusy]);

  return {
    config,
    status: sessionReadError && status !== "Running" && status !== "Starting"
      ? { Error: sessionReadError }
      : status,
    stats,
    hasSavedSession,
    sessionWarning,
    bridgePassword,
    logs,
    feed,
    subscribeActivity,
    loading,
    needsTotp,
    saveConfig,
    startBridge,
    submitTotp,
    stopBridge,
    restartBridge,
    regenerateBridgePassword,
    backupBusy,
    backupProgress,
    backupResult,
    backupError,
    startBackup,
    appVersion,
    updateReady,
    updateAvailable,
    checkUpdate,
    installUpdate,
    restartApp,
  };
}
