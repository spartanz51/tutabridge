import { useEffect, useRef } from "react";

interface Props {
  logs: string[];
  onClear: () => void;
}

export function LogsPanel({ logs, onClear }: Props) {
  const endRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    endRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [logs]);

  return (
    <div className="panel logs-panel">
      <div className="logs-header">
        <h2>Logs</h2>
        <button className="small" onClick={onClear}>
          Clear
        </button>
      </div>
      <div className="logs-container">
        {logs.length === 0 ? (
          <span className="logs-empty">No logs yet</span>
        ) : (
          logs.map((line, i) => (
            <div key={i} className="log-line">
              {line}
            </div>
          ))
        )}
        <div ref={endRef} />
      </div>
    </div>
  );
}
