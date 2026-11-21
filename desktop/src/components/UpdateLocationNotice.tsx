import { useQuery } from "@tanstack/react-query";
import { Info } from "lucide-react";
import { api } from "../lib/tauri";
import { PLAIN_COPY } from "../lib/plain-copy";

export function UpdateLocationNotice() {
  const { data: blocked } = useQuery({
    queryKey: ["update-location-block"],
    queryFn: api.updateLocationBlock,
    staleTime: Infinity,
  });
  if (!blocked) return null;
  return (
    <div
      className="connection-notice"
      role="status"
      data-testid="update-location-notice"
      style={{ marginBottom: "var(--space-4)" }}
    >
      <Info size={15} aria-hidden="true" />
      <span>{PLAIN_COPY.updates.wrongLocation}</span>
    </div>
  );
}
