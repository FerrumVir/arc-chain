import { useQuery } from "@tanstack/react-query";
import { Info } from "lucide-react";
import { api } from "../lib/tauri";
import { PLAIN_COPY } from "../lib/plain-copy";

/**
 * macOS: ARC runs from somewhere its updater can't replace it, such as the
 * mounted disk image or a copy macOS runs from a temporary read-only place.
 * Checked once when Home opens and said in one calm line: nothing is wrong
 * now and ARC stays fully usable. An update would only fail there, and ARC
 * refuses one before it stops the node.
 */
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
