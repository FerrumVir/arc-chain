// The dashboard while the node starts and catches up: the whole content area
// becomes the aqueduct, built one real step at a time.
//
// Every phase is read from the node's status by the Dashboard, never from a
// timer: checking files (start requested, no process yet), loading the model
// (process up, RPC not bound, a model configured), opening the RPC port (the
// same, with no model to load), finding peers (RPC answering, not yet live),
// catching up (the chain host is more than a few blocks ahead), and live.
// No step claims a percentage the node does not report; the only number shown
// is the real gap between the local and the chain height, or the local height.

import { ChevronDown } from "lucide-react";
import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { Aqueduct } from "./Aqueduct";
import type { ArchChainPhase } from "../lib/aqueduct/archchain";
import { formatInt } from "../lib/format";

const STEP: Record<"survey" | "model" | "rpc" | "peers", string> = {
  survey: "Checking files",
  model: "Loading the model",
  rpc: "Opening the RPC port",
  peers: "Finding peers",
};

export function StartStage({
  phase,
  localHeight,
  chainHeight,
  actions,
  onDetails,
}: {
  phase: ArchChainPhase;
  /** The node's own height; used while catching up and once live. */
  localHeight: number | null;
  /** The selected chain host's height, the target while catching up. */
  chainHeight: number | null;
  /** The node controls (Starting, Restart, Stop), rendered once, here, while the stage is up. */
  actions: ReactNode;
  /** Scroll to the rest of the dashboard. */
  onDetails: () => void;
}) {
  const stage = useRef<HTMLElement>(null);
  // The stage fills the visible content area: the scroll area's own height, measured, so it stays right with
  // the titlebar, the phone tab bar and the preview banner in or out.
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
        ? localHeight != null
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
  // One short announcement per phase; the block count is left out so it is not read aloud at every poll.
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
        // served: the running count of inference requests this node has served, once the node reports one.
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
