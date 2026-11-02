import { useEffect, useState } from "react";
import { LifeBuoy, X } from "lucide-react";
import { nodeCare, useNodeCareSnapshot } from "../lib/node-care-runtime";
import { useAppStore } from "../lib/store";

const NOTICE_BODY =
  "When your node won't start, crashes, gets stuck or an update fails, ARC sends a short anonymous report so the problem can be fixed. Once a day it also says whether the node is running. Reports never include your wallet, keys, recovery phrase, IP address or files. You can turn this off or see exactly what's sent in Settings.";

/** The one-time first-run notice. Reports wait until it has been shown. */
export function NodeCareNotice() {
  const snapshot = useNodeCareSnapshot();
  const setRoute = useAppStore((s) => s.setRoute);
  const [visible, setVisible] = useState(false);

  useEffect(() => {
    if (snapshot.ready && !snapshot.noticeShown && !visible) {
      setVisible(true);
      void nodeCare.markNoticeShown();
    }
  }, [snapshot.ready, snapshot.noticeShown, visible]);

  if (!visible) return null;
  return (
    <div
      role="status"
      data-testid="node-care-notice"
      style={{
        display: "flex",
        alignItems: "flex-start",
        gap: "var(--space-3)",
        padding: "var(--space-4) var(--space-5)",
        margin: "0 auto var(--space-5)",
        maxWidth: 1100,
        background: "var(--surface)",
        color: "var(--text)",
        border: "1px solid var(--border)",
        borderRadius: "var(--radius-md)",
      }}
    >
      <LifeBuoy size={18} style={{ color: "var(--text-muted)", flexShrink: 0, marginTop: 2 }} />
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ fontWeight: 600, marginBottom: 4 }}>Anonymous error reports are on</div>
        <div style={{ color: "var(--text-secondary)", fontSize: "var(--text-sm)", lineHeight: 1.5 }}>
          {NOTICE_BODY}
        </div>
        <div style={{ display: "flex", gap: "var(--space-2)", marginTop: "var(--space-3)" }}>
          <button className="btn btn-secondary" onClick={() => setVisible(false)} data-testid="node-care-notice-ok">
            OK
          </button>
          <button
            className="btn btn-secondary"
            onClick={() => {
              setVisible(false);
              setRoute("settings");
            }}
            data-testid="node-care-notice-settings"
          >
            Open Settings
          </button>
        </div>
      </div>
      <button
        className="btn btn-ghost"
        aria-label="Dismiss"
        onClick={() => setVisible(false)}
        style={{ padding: 4 }}
      >
        <X size={16} />
      </button>
    </div>
  );
}

function prettyBody(body: string): string {
  try {
    return JSON.stringify(JSON.parse(body), null, 2);
  } catch {
    return body;
  }
}

const DELIVERY_LABEL = {
  sent: "Sent",
  queued: "Waiting to send",
  "not-configured": "Not sent: this build has no report address",
} as const;

/** Settings: the switch, the install ID and the exact last payloads. */
export function NodeCareSettings() {
  const snapshot = useNodeCareSnapshot();
  const [showPayloads, setShowPayloads] = useState(false);

  return (
    <div style={{ display: "grid", gap: "var(--space-3)" }} data-testid="node-care-settings">
      <label style={{ display: "flex", alignItems: "flex-start", gap: "var(--space-3)", cursor: "pointer" }}>
        <input
          type="checkbox"
          checked={snapshot.enabled}
          disabled={!snapshot.ready}
          onChange={(e) => void nodeCare.setEnabled(e.target.checked)}
          data-testid="toggle-node-care"
        />
        <div>
          <div style={{ fontWeight: 500 }}>Send anonymous error reports to help fix problems</div>
          <div style={{ fontSize: "var(--text-sm)", color: "var(--text-muted)" }}>
            When the node won't start, crashes, gets stuck or an update fails, ARC sends a short report: the
            app and node versions, your operating system, what failed and a cleaned-up error message. Once a
            day it sends whether the node is running. Never your wallet, keys, recovery phrase, IP address,
            peers or files. Turning this off takes effect at once.
          </div>
          {snapshot.ready && !snapshot.endpointConfigured && (
            <div style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)", marginTop: 4 }}>
              This build has no report address, so nothing is sent.
            </div>
          )}
        </div>
      </label>

      <div style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
        Anonymous install ID: <code data-testid="node-care-install-id">{snapshot.installId || "…"}</code>
        . It's random, made on this computer, and not linked to your wallet.
      </div>

      <div style={{ display: "flex", gap: "var(--space-2)", flexWrap: "wrap" }}>
        <button
          className="btn btn-secondary"
          onClick={() => setShowPayloads((v) => !v)}
          data-testid="btn-node-care-payloads"
        >
          {showPayloads ? "Hide what's sent" : "See what's sent"}
        </button>
        <button
          className="btn btn-secondary"
          onClick={() => void nodeCare.resetInstallId()}
          disabled={!snapshot.ready}
          data-testid="btn-node-care-reset-id"
        >
          Reset install ID
        </button>
      </div>

      {showPayloads && (
        <div data-testid="node-care-payloads" style={{ display: "grid", gap: "var(--space-3)" }}>
          {snapshot.recent.length === 0 ? (
            <div style={{ fontSize: "var(--text-sm)", color: "var(--text-muted)" }}>
              Nothing has been sent yet.
            </div>
          ) : (
            snapshot.recent.map((item) => (
              <div key={`${item.at}-${item.body.length}-${item.kind}`}>
                <div style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)", marginBottom: 4 }}>
                  {item.kind === "report" ? "Error report" : "Daily heartbeat"} ·{" "}
                  {new Date(item.at).toLocaleString()} · {DELIVERY_LABEL[item.delivery]}
                </div>
                <pre
                  style={{
                    margin: 0,
                    padding: "var(--space-3)",
                    fontSize: "var(--text-xs)",
                    background: "var(--surface)",
                    border: "1px solid var(--border)",
                    borderRadius: "var(--radius-md)",
                    whiteSpace: "pre-wrap",
                    wordBreak: "break-word",
                  }}
                >
                  {prettyBody(item.body)}
                </pre>
              </div>
            ))
          )}
          {snapshot.queued > 0 && (
            <div style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
              {snapshot.queued} waiting to be sent.
            </div>
          )}
        </div>
      )}
    </div>
  );
}
