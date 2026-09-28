import { useEffect, useRef, useState, type CSSProperties, type MouseEvent as ReactMouseEvent } from "react";
import "../../lib/aqueduct/engine.js";
import "../../lib/aqueduct/archchain.js";
import type { ArchChainApi } from "../../lib/aqueduct/archchain";
import type { ChatTurn } from "../../lib/chat";
import { hostLabel } from "../../lib/hosts";
import { prefersReducedMotion } from "../Engraving";

/** What an answer has shown so far about being checked and recorded. Only observed facts, never expectations. */
export interface AnswerFacts {
  /** A second computer re-ran it and agreed (authenticated quorum). */
  agreed: boolean;
  /** Block height of an independently fetched, successful mined receipt, or null. */
  height: number | null;
}

interface Told {
  status: "pending" | "answered" | "released";
  checked: boolean;
  height: number | null;
}

/**
 * The conversation drawn as an aqueduct, one arch per answer, in the house engraving. Sending a prompt starts the next
 * arch at the end: the pier rises and the timber frame goes up; a stone hangs from the crane while no answer has come
 * back. The answer arriving sets the stones and the keystone, and the water runs into the new span. A second computer
 * agreeing knocks the frame away; a mined receipt cuts its block into the arch's tablet. Every step is a real event of
 * the turn; nothing is drawn ahead of it.
 */
export function ChatAqueduct({
  turns,
  facts,
  onPick,
  style,
}: {
  turns: ChatTurn[];
  facts: Record<string, AnswerFacts>;
  /** Called with a turn id when its arch is clicked. */
  onPick?: (turnId: string) => void;
  style?: CSSProperties;
}) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const far = useRef<HTMLDivElement>(null);
  const near = useRef<HTMLDivElement>(null);
  const api = useRef<ArchChainApi | null>(null);
  const arch = useRef(new Map<string, number>());
  const told = useRef(new Map<string, Told>());
  const latest = useRef({ turns, facts });
  latest.current = { turns, facts };
  const [tip, setTip] = useState<{ x: number; lines: [string, string] } | null>(null);

  useEffect(() => {
    if (!canvas.current) return;
    const a = window.ArchChain.mount(canvas.current, { chat: true, reduced: prefersReducedMotion(), far: far.current ?? undefined, near: near.current ?? undefined });
    api.current = a;
    // the conversation so far: its answered turns are finished arches; a turn still waiting is the arch being built
    const { turns: ts, facts: fs } = latest.current;
    const done = ts.filter((t) => t.status === "answered");
    a.load(done.map((t) => ({ checked: fs[t.id]?.agreed ? true : null, height: fs[t.id]?.height ?? null })));
    done.forEach((t, i) => {
      arch.current.set(t.id, i);
      told.current.set(t.id, { status: "answered", checked: Boolean(fs[t.id]?.agreed), height: fs[t.id]?.height ?? null });
    });
    const waiting = ts.find((t) => t.status === "pending");
    if (waiting) {
      arch.current.set(waiting.id, a.ask());
      told.current.set(waiting.id, { status: "pending", checked: false, height: null });
    }
    return () => {
      a.destroy();
      api.current = null;
      arch.current = new Map();
      told.current = new Map();
    };
  }, []);

  useEffect(() => {
    const a = api.current;
    if (!a) return;
    if (turns.length === 0) {
      if (told.current.size) {
        a.load([]);
        arch.current = new Map();
        told.current = new Map();
      }
      return;
    }
    for (const t of turns) {
      const was = told.current.get(t.id);
      if (!was) {
        if (t.status !== "pending") continue;
        // only one arch is built at a time: a turn the person stopped waiting for gives its site to the new prompt
        for (const [id, w] of told.current) {
          if (w.status === "pending") {
            a.failed();
            w.status = "released";
            arch.current.delete(id);
          }
        }
        arch.current.set(t.id, a.ask());
        told.current.set(t.id, { status: "pending", checked: false, height: null });
        continue;
      }
      if (was.status === "pending" && t.status === "answered") {
        a.answered();
        was.status = "answered";
      } else if (was.status === "pending" && t.status === "failed") {
        a.failed();
        was.status = "released";
        arch.current.delete(t.id);
      }
      const i = arch.current.get(t.id);
      const f = facts[t.id];
      if (i == null || was.status !== "answered" || !f) continue;
      if (f.agreed && !was.checked) {
        a.checked(true, i);
        was.checked = true;
      }
      if (f.height != null && was.height == null) {
        a.recorded(f.height, i);
        was.height = f.height;
      }
    }
  }, [turns, facts]);

  const turnAt = (clientX: number, clientY: number) => {
    const a = api.current;
    const c = canvas.current;
    if (!a || !c) return null;
    const r = c.getBoundingClientRect();
    const i = a.hit(clientX - r.left, clientY - r.top);
    if (typeof i !== "number" || i < 0) return null;
    for (const [id, n] of arch.current) if (n === i) return { turn: turns.find((t) => t.id === id) ?? null, index: i, x: clientX - r.left, width: r.width };
    return null;
  };
  const onMove = (e: ReactMouseEvent) => {
    const hit = turnAt(e.clientX, e.clientY);
    const t = hit?.turn;
    if (!hit || !t) {
      setTip(null);
      return;
    }
    const f = facts[t.id];
    const by = t.result ? (t.result.servedLocally ? "your node" : t.result.coordinator ? hostLabel(t.result.coordinator) : "the network") : null;
    const state = t.status === "pending" ? "waiting for an answer" : `served by ${by}`;
    const checked = f?.agreed ? "checked by a second computer" : "not checked by a second computer";
    const chain = f?.height != null ? `block ${f.height.toLocaleString("en-US")}` : "not on chain yet";
    const prompt = t.prompt.length > 64 ? `${t.prompt.slice(0, 63)}…` : t.prompt;
    setTip({ x: Math.max(8, Math.min(hit.width - 300, hit.x + 14)), lines: [`Answer ${hit.index + 1} · ${state}${t.status === "answered" ? ` · ${checked} · ${chain}` : ""}`, prompt] });
  };
  const onClick = (e: ReactMouseEvent) => {
    const t = turnAt(e.clientX, e.clientY)?.turn;
    if (t && onPick) onPick(t.id);
  };

  const answered = turns.filter((t) => t.status === "answered");
  const label =
    answered.length === 0
      ? "Your conversation, drawn as an aqueduct: no answers yet. Each answer will add an arch."
      : `Your conversation, drawn as an aqueduct: ${answered.length} ${answered.length === 1 ? "answer" : "answers"}, ${
          answered.filter((t) => facts[t.id]?.agreed).length
        } checked by a second computer, ${answered.filter((t) => facts[t.id]?.height != null).length} recorded on chain.`;
  const fill: CSSProperties = { position: "absolute", inset: 0, width: "100%", height: "100%" };
  return (
    <div className="chat-band" style={style} data-testid="chat-aqueduct">
      <div ref={far} style={{ ...fill, pointerEvents: "none" }} aria-hidden="true" />
      <div ref={near} style={{ ...fill, pointerEvents: "none" }} aria-hidden="true" />
      <canvas
        ref={canvas}
        style={{ ...fill, display: "block", cursor: tip ? "pointer" : "default" }}
        role="img"
        aria-label={label}
        onMouseMove={onMove}
        onMouseLeave={() => setTip(null)}
        onClick={onClick}
      />
      {tip && (
        <div className="chat-band-tip" style={{ left: tip.x }} aria-hidden="true">
          <span>{tip.lines[0]}</span>
          <span className="chat-band-tip-prompt">{tip.lines[1]}</span>
        </div>
      )}
    </div>
  );
}
