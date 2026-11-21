import { ChevronDown } from "lucide-react";
import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { Aqueduct } from "./Aqueduct";
import type { ArchChainPhase } from "../lib/aqueduct/archchain";
import { formatInt } from "../lib/format";

const STEP: Record<"survey" | "model" | "rpc" | "peers", string> = {
  survey: "Checking files",
  model: "Loading the model",
  rpc: "Opening the RPC port",
  peers: "Connecting to the ARC network",
};

export function StartStage({
  phase,
  localHeight,
  chainHeight,
  actions,
  onDetails,
}: {
  phase: ArchChainPhase;
  localHeight: number | null;
  chainHeight: number | null;
  actions: ReactNode;
  onDetails: () => void;
}) {
  const stage = useRef<HTMLElement>(null);
  const [areaHeight, setAreaHeight] = useState<number | null>(null);
  useLayoutEffect(() => {
    const main = stage.current?.closest(".main");
    if (!(main instanceof HTMLElement)) return;
    const measure = () => setAreaHeight(main.clientHeight);
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(measure);
    ro.observe(main);
    return () => ro.disconnect();
  }, []);

  const toGo = localHeight != null && chainHeight != null ? Math.max(0, chainHeight - localHeight) : null;
  const title = phase === "sync" ? "Catching up" : phase === "live" ? "Your node is live" : "Starting your node";
  const step =
    phase === "sync"
      ? toGo != null
        ? `${formatInt(toGo)} ${toGo === 1 ? "block" : "blocks"} to go`
        : "Catching up with the chain"
      : phase === "live"
        ? localHeight != null && localHeight > 0
          ? `Block ${formatInt(localHeight)}`
          : "Connected"
        : STEP[phase];
  const label =
    phase === "sync"
      ? `The node catching up, drawn as an aqueduct being built${
          localHeight != null && chainHeight != null ? `: block ${formatInt(localHeight)} of ${formatInt(chainHeight)}` : ""
        }.`
      : phase === "live"
        ? `The node is live${localHeight != null ? ` at block ${formatInt(localHeight)}` : ""}, drawn as the aqueduct it is building.`
        : `The node starting, drawn as the first arch of an aqueduct being built: ${STEP[phase].toLowerCase()}.`;
  const announcement = phase === "sync" ? "Catching up with the chain." : phase === "live" ? "Your node is live." : `Starting your node: ${STEP[phase].toLowerCase()}.`;

  return (
    <section
      ref={stage}
      className="start-stage"
      data-testid="start-stage"
      data-phase={phase}
      aria-label="Node start-up"
      style={areaHeight ? { height: Math.max(320, areaHeight) } : undefined}
    >
      <Aqueduct
        className="start-stage-art"
        bot={345}
        phase={phase}
        progress={null}
        height={phase === "sync" || phase === "live" ? localHeight : null}
        label={label}
      />
      <div className="start-stage-top">
        <div className="start-stage-copy">
          <h2 className="start-stage-title">{title}</h2>
          <p className="start-stage-step" data-testid="start-stage-step">
            {step}
          </p>
        </div>
        <div className="start-stage-actions">{actions}</div>
      </div>
      <p className="sr-only" role="status" aria-live="polite">
        {announcement}
      </p>
      <button type="button" className="start-stage-more" onClick={onDetails}>
        Node details <ChevronDown size={14} aria-hidden="true" />
      </button>
    </section>
  );
}
