import type { BackupStats, BackupProgress } from "../hooks/useBridge";

interface Props {
  isRunning: boolean;
  busy: boolean;
  progress: BackupProgress | null;
  result: BackupStats | null;
  error: string | null;
  onBackup: () => void;
}

export function BackupPanel({ isRunning, busy, progress, result, error, onBackup }: Props) {
  const pct =
    progress && progress.total > 0
      ? Math.round((progress.done / progress.total) * 100)
      : 0;

  return (
    <div className="dashboard">
      <div className="panel">
        <h2>Backup mailbox</h2>
        <p className="muted">
          Export <strong>every</strong> email to a folder of <code>.eml</code> files —
          your complete mailbox, not just the messages currently synced. The files
          open in any mail client (Thunderbird, Apple Mail, Outlook). Re-running into
          the same folder resumes where it left off and only fetches new mail.
        </p>

        {!isRunning && (
          <p className="muted" style={{ color: "var(--orange)" }}>
            Start the bridge first — backup reuses its signed-in session.
          </p>
        )}

        <button
          className="primary"
          disabled={!isRunning || busy}
          onClick={onBackup}
          style={{ marginTop: "0.75rem" }}
        >
          {busy ? "Backing up…" : "Choose folder & back up"}
        </button>

        {busy && progress && (
          <div style={{ marginTop: "1rem" }}>
            <div className="muted" style={{ marginBottom: "0.35rem" }}>
              {progress.folder}: {progress.done} / {progress.total} ({pct}%)
            </div>
            <progress
              value={progress.done}
              max={progress.total}
              style={{ width: "100%" }}
            />
          </div>
        )}

        {busy && !progress && (
          <p className="muted" style={{ marginTop: "1rem" }}>
            Enumerating mail…
          </p>
        )}

        {result && (
          <div style={{ marginTop: "1.25rem" }}>
            <div className="stats-grid">
              <div className="stat-card">
                <div className="stat-value">{result.mails_written}</div>
                <div className="stat-label">mails written</div>
              </div>
              <div className="stat-card">
                <div className="stat-value">{result.folders}</div>
                <div className="stat-label">folders</div>
              </div>
              <div className="stat-card">
                <div className="stat-value">
                  {(result.bytes / 1_000_000).toFixed(1)} MB
                </div>
                <div className="stat-label">on disk</div>
              </div>
            </div>
            <p className="muted" style={{ marginTop: "0.75rem" }}>
              {result.from_cache} from local cache, {result.from_server} fetched
              from the server
              {result.skipped > 0
                ? `, ${result.skipped} skipped (already backed up)`
                : ""}
              .
            </p>
            {result.errors.length > 0 && (
              <p className="muted" style={{ color: "var(--orange)" }}>
                {result.errors.length} mail(s) could not be exported.
              </p>
            )}
          </div>
        )}

        {error && (
          <p className="muted" style={{ color: "var(--red)", marginTop: "1rem" }}>
            Backup failed: {error}
          </p>
        )}
      </div>
    </div>
  );
}
