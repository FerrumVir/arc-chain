// What happened to a prompt, in plain words: sent, served, checked, on chain.
// Each stage shows only what the app observed or the node returned. A stage
// the app has no evidence for yet stays open; it is never marked done ahead
// of the event.

import type { ReactNode } from "react";

/** done: observed. wait: under way, or submitted but not confirmed. open: nothing yet. */
export type StageState = "done" | "wait" | "open";

export interface Stage {
  name: string;
  state: StageState;
  detail: ReactNode;
}

export function StageTrack({ label, stages }: { label: string; stages: Stage[] }) {
  return (
    <ol className="stages" aria-label={label}>
      {stages.map((stage) => (
        <li key={stage.name} className={`stage ${stage.state}`}>
          <span className="stage-name">{stage.name}</span>
          <span className="stage-detail">{stage.detail}</span>
        </li>
      ))}
    </ol>
  );
}

export function timeOf(ms: number) {
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}
