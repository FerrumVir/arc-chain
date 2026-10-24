import { useMutation, useQueryClient } from "@tanstack/react-query";
import { RotateCw } from "lucide-react";
import { api } from "../lib/tauri";
import { useAppStore } from "../lib/store";
import { PLAIN_COPY } from "../lib/plain-copy";
import { ProblemNotice } from "./ProblemNotice";

// Shown when the Rust side reports that our child arc-node process died on
// its own: one plain sentence, one button to start it again, and the node's
// exit text behind "Show details". Dismiss clears the crash state without
// restarting. Through v0.8.14 the exit text was the body of the banner.
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
    <ProblemNotice
      testId="crash-banner"
      title={PLAIN_COPY.nodeStrip.crashed}
      body={PLAIN_COPY.dashboard.crashedBody}
      details={message}
      actions={
        <button
          type="button"
          className="btn btn-primary btn-sm"
          onClick={() => relaunch.mutate()}
          disabled={relaunch.isPending}
          aria-busy={relaunch.isPending || undefined}
          data-testid="btn-crash-relaunch"
        >
          <RotateCw size={12} className={relaunch.isPending ? "spin" : undefined} />{" "}
          {PLAIN_COPY.actions.startAgain}
        </button>
      }
      onDismiss={() => dismiss.mutate()}
      dismissLabel="Dismiss crash"
      dismissTestId="btn-crash-dismiss"
    />
  );
}
