import { useMutation, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, RotateCw, X } from "lucide-react";
import { api } from "../lib/tauri";
import { useAppStore } from "../lib/store";

// Surfaces an inline banner whenever the Rust side reports that our child
// arc-node process died on its own. One-click relaunch; dismiss clears the
// crash state without restarting.
export function CrashBanner({ message }: { message: string }) {
  const queryClient = useQueryClient();
  const config = useAppStore((s) => s.config);

  const relaunch = useMutation({
    mutationFn: async () => {
      await api.clearCrash();
      if (config) await api.startNode(config);
    },
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["status"] }),
  });

  const dismiss = useMutation({
    mutationFn: () => api.clearCrash(),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["status"] }),
  });

  return (
    <div
      role="alert"
      data-testid="crash-banner"
      style={{
        display: "flex",
        gap: "var(--space-3)",
        alignItems: "flex-start",
        padding: "var(--space-4) var(--space-5)",
        background: "var(--danger-bg)",
        border: "1px solid rgba(240, 115, 115, 0.3)",
        borderRadius: "var(--radius-md)",
        marginBottom: "var(--space-5)",
      }}
    >
      <AlertTriangle
        size={18}
        style={{ color: "var(--danger)", flexShrink: 0, marginTop: 1 }}
      />
      <div style={{ flex: 1, minWidth: 0 }}>
        <div
          style={{
            fontWeight: 500,
            color: "var(--text)",
            marginBottom: 2,
          }}
        >
          Your node crashed
        </div>
        <div
          style={{
            fontSize: "var(--text-sm)",
            color: "var(--text-muted)",
            fontFamily: "var(--font-mono)",
            lineHeight: 1.5,
          }}
        >
          {message}
        </div>
      </div>
      <div style={{ display: "flex", gap: "var(--space-2)" }}>
        <button
          className="btn btn-secondary btn-sm"
          onClick={() => relaunch.mutate()}
          disabled={relaunch.isPending}
          data-testid="btn-crash-relaunch"
        >
          <RotateCw size={12} /> Relaunch
        </button>
        <button
          className="btn btn-ghost btn-sm"
          onClick={() => dismiss.mutate()}
          aria-label="Dismiss crash"
          data-testid="btn-crash-dismiss"
          style={{ padding: 6 }}
        >
          <X size={12} />
        </button>
      </div>
    </div>
  );
}
