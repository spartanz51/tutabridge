import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Config, BridgeStatus, BridgeStats } from "../types";

const MAX_LOG_LINES = 500;

export function useBridge() {
  const [config, setConfig] = useState<Config | null>(null);
  const [status, setStatus] = useState<BridgeStatus | null>(null);
  const [stats, setStats] = useState<BridgeStats>({
    uptime_secs: null,
    mails_synced: 0,
    ws_status: "Stopped",
  });
  const [hasSavedSession, setHasSavedSession] = useState(false);
  const [bridgePassword, setBridgePassword] = useState<string | null>(null);
  const [logs, setLogs] = useState<string[]>([]);
  const [loading, setLoading] = useState(false);

  const refresh = useCallback(() => {
    invoke<BridgeStatus>("get_status").then(setStatus);
    invoke<BridgeStats>("get_stats").then(setStats);
  }, []);

  useEffect(() => {
    invoke<Config>("get_config").then(setConfig);
    invoke<boolean>("has_saved_session").then(setHasSavedSession);
    invoke<string | null>("get_bridge_password").then(setBridgePassword);
    refresh();

    // The bridge pushes `bridge://stats` and `bridge://status` whenever
    // anything changes (mail count, ws state, start/stop). No setInterval.
    const unlistenStats = listen<BridgeStats>("bridge://stats", (e) => setStats(e.payload));
    const unlistenStatus = listen<BridgeStatus>("bridge://status", (e) => setStatus(e.payload));
    return () => {
      unlistenStats.then((fn) => fn());
      unlistenStatus.then((fn) => fn());
    };
  }, [refresh]);

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

  const saveConfig = useCallback(async (cfg: Config) => {
    await invoke("save_config", { config: cfg });
    setConfig(cfg);
  }, []);

  const startBridge = useCallback(async (password?: string) => {
    setLoading(true);
    try {
      await invoke("start_bridge", { password: password || null });
      refresh();
      invoke<string | null>("get_bridge_password").then(setBridgePassword);
    } catch (e) {
      setStatus({ Error: String(e) });
    } finally {
      setLoading(false);
    }
  }, [refresh]);

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
      refresh();
    } catch (e) {
      setStatus({ Error: String(e) });
    } finally {
      setLoading(false);
    }
  }, [refresh]);

  const clearLogs = useCallback(() => setLogs([]), []);

  const regenerateBridgePassword = useCallback(async () => {
    const newPassword = await invoke<string>("regenerate_bridge_password");
    setBridgePassword(newPassword);
    return newPassword;
  }, []);

  return {
    config,
    status,
    stats,
    hasSavedSession,
    bridgePassword,
    logs,
    loading,
    saveConfig,
    startBridge,
    stopBridge,
    restartBridge,
    clearLogs,
    regenerateBridgePassword,
  };
}
