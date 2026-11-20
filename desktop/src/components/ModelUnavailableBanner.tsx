import { useAppStore } from "../lib/store";
import type { ModelUnavailableReason } from "../lib/types";

export function ModelUnavailableBanner({
  reason,
}: {
  reason: ModelUnavailableReason;
}) {
  const setRoute = useAppStore((s) => s.setRoute);
  const notFound = reason === "notFound";
  return (
    <div
      className="lite-banner"
      role="status"
      data-testid="model-unavailable-banner"
      data-reason={reason}
    >
      <strong>{notFound ? "Model file not found" : "Model file can't be read"}</strong>{" "}
      — so local compute is off. Your node keeps running as an observer.{" "}
      {notFound
        ? "Restore the file, or re-download the model in Settings."
        : "Check the file's permissions, or re-download the model in Settings."}
      <div style={{ marginTop: "var(--space-3)" }}>
        <button
          type="button"
          className="btn btn-secondary"
          data-testid="btn-model-unavailable-settings"
          onClick={() => setRoute("settings")}
        >
          Open Settings
        </button>
      </div>
    </div>
  );
}
