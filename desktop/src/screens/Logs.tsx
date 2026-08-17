import { useMutation, useQuery } from "@tanstack/react-query";
import { Download, ScrollText } from "lucide-react";
import { useEffect, useRef } from "react";
import { Card, CardHeader } from "../components/Card";
import { EmptyState } from "../components/EmptyState";
import { api } from "../lib/tauri";

export function Logs() {
  const { data: logs } = useQuery({
    queryKey: ["logs"],
    queryFn: () => api.fetchLogs(500),
    refetchInterval: 2000,
  });

  const { data: status } = useQuery({
    queryKey: ["status"],
    queryFn: api.nodeStatus,
    refetchInterval: 3000,
  });

  const isExternal = status?.running && status?.pid == null;

  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [logs]);

  // The write happens in Rust behind a native save dialog.
  //
  // This used to build a Blob and click a synthetic `<a download>`.
  // WKWebView (the macOS webview) doesn't implement the download attribute
  // for blob: URLs without a host-side download delegate, so the button was
  // a silent no-op on macOS while working on Windows and Linux. Handing logs
  // to support is the entire purpose of this button.
  const save = useMutation({
    mutationFn: () => api.saveLogs(),
  });

  return (
    <div className="main-inner" data-testid="logs-screen">
      <div className="page-header">
        <div>
          <h1 className="page-title">Logs</h1>
          <p className="page-subtitle">
            Live output from your node process. Useful for debugging.
          </p>
        </div>
        <div style={{ textAlign: "right" }}>
          <button
            className="btn btn-secondary"
            onClick={() => save.mutate()}
            disabled={!logs?.length || save.isPending}
            data-testid="btn-download-logs"
          >
            <Download size={14} />{" "}
            {save.isPending ? "Saving…" : "Save logs"}
          </button>
          {save.data?.path && (
            <p
              style={{
                marginTop: "var(--space-2)",
                fontSize: "var(--text-sm)",
                color: "var(--text-muted)",
                wordBreak: "break-all",
              }}
              data-testid="logs-saved-path"
            >
              Saved {save.data.lines} lines to {save.data.path}
            </p>
          )}
          {save.error && (
            <p
              style={{
                marginTop: "var(--space-2)",
                fontSize: "var(--text-sm)",
                color: "var(--danger)",
              }}
              data-testid="logs-save-error"
            >
              {String(save.error)}
            </p>
          )}
        </div>
      </div>

      <Card>
        <CardHeader title="Console" />
        <div className="log-console" ref={scrollRef} data-testid="log-console">
          {!logs || logs.length === 0 ? (
            isExternal ? (
              <EmptyState
                icon={ScrollText}
                title="Node managed externally"
                description="Your node is running under launchd or systemd, so its stdout goes straight to system logs. Run the node via this app (Settings → Start node on app launch) to see logs here."
              />
            ) : (
              <EmptyState
                icon={ScrollText}
                title="No logs yet"
                description="Logs appear when the node starts."
              />
            )
          ) : (
            logs.map((l) => (
              <div key={l.id} className="log-line">
                <span className="log-time">
                  {new Date(l.timestamp).toLocaleTimeString("en-US", {
                    hour12: false,
                  })}
                </span>
                <span className={`log-level-${l.level}`}>
                  {l.level.padEnd(5).toUpperCase()}
                </span>
                <span>{l.message}</span>
              </div>
            ))
          )}
        </div>
      </Card>
    </div>
  );
}
