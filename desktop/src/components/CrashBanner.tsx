import { useMutation, useQueryClient } from "@tanstack/react-query";
import { RotateCw } from "lucide-react";
import { api } from "../lib/tauri";
import { PLAIN_COPY } from "../lib/plain-copy";
import { ProblemNotice } from "./ProblemNotice";

/**
 * Shown when the Rust side reports that our child arc-node process died on
 * its own: one plain sentence, one button to start it again, and the node's
 * exit text behind "Show details". Dismiss clears the crash state without
 * restarting. Through v0.8.14 the exit text was the body of the banner.
 *
 * The start itself belongs to the Dashboard (`onStartAgain`): clearing the
 * crash unmounts this banner, so an error kept here would vanish with it.
 * The Dashboard keeps that error the way Home does and shows it as the
 * start-failure panel.
 */
export function CrashBanner({
  message,
  onStartAgain,
  starting,
}: {
  message: string;
  onStartAgain: () => void;
  starting: boolean;
}) {
  const queryClient = useQueryClient();

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
          onClick={onStartAgain}
          disabled={starting}
          aria-busy={starting || undefined}
          data-testid="btn-crash-relaunch"
        >
          <RotateCw size={12} className={starting ? "spin" : undefined} />{" "}
          {PLAIN_COPY.actions.startAgain}
        </button>
      }
      onDismiss={() => dismiss.mutate()}
      dismissLabel="Dismiss crash"
      dismissTestId="btn-crash-dismiss"
    />
  );
}
