import { Check, CircleStop, Copy, Loader2, Send, SquarePen } from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { InfoPopover } from "../components/InfoPopover";
import { NativePaidRequests } from "../components/NativePaidRequests";
import { NodeStatusStrip } from "../components/NodeStatusStrip";
import { PLAIN_COPY } from "../lib/plain-copy";
import { Answer } from "../components/chat/Answer";
import { StageTrack, timeOf } from "../components/chat/Stages";
import { ChatAqueduct, type AnswerFacts } from "../components/chat/ChatAqueduct";
import { prefersReducedMotion } from "../components/Engraving";
import { api } from "../lib/tauri";
import { hostLabel } from "../lib/hosts";
import { earlierSameAsk, newTurnId, useChatStore, type ChatTurn, type DispatchRoute } from "../lib/chat";
import type { InferenceResult } from "../lib/types";

const EXAMPLES = ["The largest planet is", "Water boils at", "The sun is a", "Bitcoin is a"];

// This exact native/browser sentinel is the only proof that no inference POST
// was sent. Every other error may be an ambiguous accepted write and is
// terminal for this click.
const INFERENCE_PRE_DISPATCH_UNAVAILABLE = "ARC_INFERENCE_PRE_DISPATCH_UNAVAILABLE:";

/// Local execution stays first. If this machine cannot serve, call a seed's
/// `/inference/run` before the standalone consensus route: that endpoint gives
/// registered community workers first refusal, then safely falls through to
/// the seed's sharded/local execution. Only when every direct coordinator
/// fails before completing a job do we use `/inference/run_consensus`.
/// `onRoute` reports each route as this app dispatches to it: the app's own
/// decisions, never a claim about the node's progress.
async function runInferenceSmart(prompt: string, maxTokens: number, onRoute: (route: DispatchRoute) => void): Promise<InferenceResult> {
  const fallbackToCoordinator = async (): Promise<InferenceResult> => {
    try {
      onRoute("direct");
      return await api.runInferenceViaCoordinatorDirect(prompt, maxTokens);
    } catch (directErr) {
      const message = String(directErr instanceof Error ? directErr.message : directErr);
      // The native/browser direct path may inspect multiple read-only
      // readiness endpoints, but emits this typed sentinel only if it sent no
      // POST at all. Never infer write safety from 503 text, connection
      // errors, or a timeout: any of those can arrive after acceptance.
      if (message.startsWith(INFERENCE_PRE_DISPATCH_UNAVAILABLE)) {
        onRoute("consensus");
        return await api.runInferenceViaCoordinator(prompt, maxTokens);
      }
      throw directErr;
    }
  };

  try {
    onRoute("local");
    // An empty successful completion can be a legitimate immediate EOS. Do
    // not duplicate it on another coordinator merely to manufacture text.
    return await api.runInference(prompt, maxTokens);
  } catch (err) {
    const msg = String(err instanceof Error ? err.message : err);
    // A mutation-free readiness failure is the only safe reason to migrate
    // the click. Any response/parse/reset/timeout after a local POST is
    // terminal even when its text happens to mention 503 or "connection".
    if (msg.startsWith(INFERENCE_PRE_DISPATCH_UNAVAILABLE)) {
      return await fallbackToCoordinator();
    }
    throw err;
  }
}

// What the thread says while a question is out. A community node cannot
// take questions itself (it has no peers and usually no model loaded), so
// going to the network is the normal path, not a shortfall of "your node".
const ROUTE_WAITING: Record<DispatchRoute, string> = {
  local: PLAIN_COPY.ask.askingNode,
  direct: PLAIN_COPY.ask.askingNetwork,
  consensus: PLAIN_COPY.ask.askingNetwork,
};
const ROUTE_SHORT: Record<DispatchRoute, string> = { local: "your node", direct: "the ARC network", consensus: "a network consensus run" };

// After this long the waiting note says the network is slow today.
const SLOW_AFTER_SECS = 60;

function formatElapsed(seconds: number): string {
  if (seconds < 60) return `${seconds} s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes} min ${seconds % 60} s`;
}

/** Why a question got no answer, from the error the dispatch returned. The text itself stays behind Show details. */
function describeFailure(error: string | undefined) {
  const raw = error ?? "";
  const copy = PLAIN_COPY.ask;
  // This exact sentinel is the only proof that no inference POST was sent.
  if (raw.startsWith(INFERENCE_PRE_DISPATCH_UNAVAILABLE)) {
    return { kind: "not-sent" as const, title: copy.notSent, note: copy.notSentNote, retry: PLAIN_COPY.actions.tryAgain, raw };
  }
  if (/OUTCOME_AMBIGUOUS|may still settle/i.test(raw)) {
    return { kind: "unknown-outcome" as const, title: copy.unknownOutcome, note: copy.unknownOutcomeNote, retry: copy.sendAgainAnyway, raw };
  }
  return { kind: "no-answer" as const, title: copy.noAnswer, note: copy.noAnswerNote, retry: PLAIN_COPY.actions.tryAgain, raw };
}

/** One short line for screen readers about the newest turn, instead of reading whole answers aloud. */
function announce(turns: ChatTurn[]): string {
  const t = turns[turns.length - 1];
  if (!t) return "";
  if (t.status === "pending") return t.detached ? "Stopped waiting. A late answer will still appear." : t.route ? ROUTE_WAITING[t.route] : PLAIN_COPY.ask.sending;
  if (t.status === "failed") return describeFailure(t.error).title;
  const r = t.result;
  if (!r) return "";
  const by = r.servedLocally ? "your node" : r.coordinator ? hostLabel(r.coordinator) : "the network";
  const checked = r.quorumVerified === true && r.profileBound === true ? "checked by a second computer" : "not checked by a second computer";
  return `Answer received from ${by}, ${checked}.`;
}

export function Inference() {
  const turns = useChatStore((s) => s.turns);
  const addTurn = useChatStore((s) => s.addTurn);
  const updateTurn = useChatStore((s) => s.updateTurn);
  const clear = useChatStore((s) => s.clear);
  const [draft, setDraft] = useState("");
  const [maxTokens, setMaxTokens] = useState(16);
  // React's disabled prop is applied only after the next render. Two click or
  // keyboard activations in the same event turn can otherwise start two
  // requests, and therefore two reward-capable POSTs. This synchronous latch
  // closes before the request starts and reopens only when it settles (or when
  // the person explicitly stops waiting for it, in a later event).
  const latch = useRef(false);
  const activeId = useRef<string | null>(null);
  const composer = useRef<HTMLTextAreaElement>(null);
  const threadEnd = useRef<HTMLDivElement>(null);
  // what each answer has shown about being checked and recorded, for the chat aqueduct
  const [facts, setFacts] = useState<Record<string, AnswerFacts>>({});
  const noteFacts = (id: string, f: AnswerFacts) =>
    setFacts((prev) => (prev[id]?.agreed === f.agreed && prev[id]?.height === f.height ? prev : { ...prev, [id]: f }));
  const pick = (id: string) => {
    document.querySelector(`[data-turn-id="${id}"]`)?.scrollIntoView({ block: "start", behavior: prefersReducedMotion() ? "auto" : "smooth" });
  };
  const waiting = turns.some((t) => t.status === "pending" && !t.detached);
  const anyInFlight = turns.some((t) => t.status === "pending");

  const send = (text: string, tokens: number): boolean => {
    const prompt = text.trim();
    if (!prompt || latch.current) return false;
    latch.current = true;
    const id = newTurnId();
    activeId.current = id;
    addTurn({ id, prompt, maxTokens: tokens, sentAt: Date.now(), status: "pending", route: null, detached: false });
    runInferenceSmart(prompt, tokens, (route) => updateTurn(id, { route }))
      .then((result) => updateTurn(id, { status: "answered", result, settledAt: Date.now() }))
      .catch((err) => updateTurn(id, { status: "failed", error: err instanceof Error ? err.message : String(err), settledAt: Date.now() }))
      .finally(() => {
        if (activeId.current === id) {
          activeId.current = null;
          latch.current = false;
        }
      });
    return true;
  };
  const submit = () => {
    if (send(draft, maxTokens)) setDraft("");
  };
  const stopWaiting = () => {
    const id = activeId.current;
    if (!id) return;
    updateTurn(id, { detached: true });
    activeId.current = null;
    latch.current = false;
  };

  // bring the newest turn into view as the thread grows or a turn settles: its top, so a long answer reads
  // from its first line (the browser stops at the end of the thread, above the pinned composer)
  const lastKey = turns.length ? `${turns.length}:${turns[turns.length - 1].status}` : "0";
  useEffect(() => {
    if (!turns.length) return;
    const last = threadEnd.current?.previousElementSibling;
    last?.scrollIntoView({ block: "start", behavior: prefersReducedMotion() ? "auto" : "smooth" });
  }, [lastKey, turns.length]);
  // the composer grows with its text, up to a limit, then scrolls
  useLayoutEffect(() => {
    const el = composer.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 180)}px`;
  }, [draft]);

  return (
    <div className="main-inner chat-screen" data-testid="inference-screen">
      <div className="page-header chat-header">
        <div>
          <h1 className="page-title">{PLAIN_COPY.home.title}</h1>
          <p className="page-subtitle">Every answer shows who served it, whether a second computer checked it, and what reached the chain.</p>
        </div>
        <div className="chat-header-actions">
          <InfoPopover title="How this works">
            <p>
              Your prompt goes to your own node first, at <code>POST /inference/run</code> on <code>127.0.0.1</code>. If your node isn&rsquo;t running or
              has no compatible model loaded, it falls back to a reachable coordinator&rsquo;s community-first <code>/inference/run</code> route. Either
              way the answer says where the compute ran and what the coordinator actually verified.
            </p>
            <p>1. Attempts the prompt on the selected execution path. A trace shows shard hops only when the coordinator reports one.</p>
            <p>
              2. Returns the reported output commitment and model ID. On the protocol-v3 path the model ID hashes every artifact byte. Older nodes may
              report only a shape-derived ID, which is not exact artifact identity.
            </p>
            <p>
              3. The serving coordinator may submit an <code>InferenceAttestation</code> (<code>0x16</code>) with{" "}
              <code>(input_hash, output_hash, model_hash)</code>. It is a computation claim, not a payment or proof of correctness.
            </p>
            <p>
              4. If a claim hash is returned, the in-app lookup can confirm whether this host mined it successfully. Community payment is a separate{" "}
              <code>0x25</code> transaction and is never inferred from this result.
            </p>
          </InfoPopover>
          <button type="button" className="btn btn-secondary btn-sm" onClick={clear} disabled={anyInFlight || turns.length === 0} data-testid="btn-new-conversation">
            <SquarePen size={14} /> New conversation
          </button>
        </div>
      </div>

      {/* Home: the node's state and its one fix. The chat below answers from
          the network whatever this says. */}
      <NodeStatusStrip />

      <ChatAqueduct turns={turns} facts={facts} onPick={pick} />

      {/* Renders only when the pinned host's chain is a protocol-4 chain. */}
      <NativePaidRequests />

      <p className="sr-only" role="status" aria-live="polite" data-testid="inference-announce">
        {announce(turns)}
      </p>
      {/* A band for the whole conversation sits here, full width of the chat column and above the thread
          (see .chat-band in chat.css). The column is a flex column, so the band needs no other change. */}
      <section className="thread" aria-label="Conversation">
        {turns.length === 0 ? (
          <EmptyThread />
        ) : (
          turns.map((turn, i) => {
            const latest = i === turns.length - 1;
            return (
              <div className="turn" key={turn.id} data-turn-id={turn.id}>
                <div className="ask">
                  <div className="ask-bubble">{turn.prompt}</div>
                  <div className="ask-meta">
                    {new Date(turn.sentAt).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })} · up to {turn.maxTokens} tokens
                  </div>
                </div>
                {turn.status === "pending" && <Pending turn={turn} />}
                {turn.status === "failed" && <Failed turn={turn} latest={latest} onRetry={() => send(turn.prompt, turn.maxTokens)} disabled={waiting} />}
                {turn.status === "answered" && (
                  <Answer
                    turn={turn}
                    latest={latest}
                    earlier={earlierSameAsk(turns, turn)}
                    onAskAgain={() => send(turn.prompt, turn.maxTokens)}
                    onFacts={(f) => noteFacts(turn.id, f)}
                  />
                )}
              </div>
            );
          })
        )}
        <div ref={threadEnd} />
      </section>

      <div className="composer" role="group" aria-label={PLAIN_COPY.home.composerLabel}>
        <div className="composer-box">
          {turns.length === 0 && (
            <div className="composer-suggest">
              <span>Try</span>
              {EXAMPLES.map((ex) => (
                <button
                  key={ex}
                  type="button"
                  className="chip-btn chip-example"
                  onClick={() => {
                    setDraft(ex);
                    composer.current?.focus();
                  }}
                  data-testid={`example-${ex.slice(0, 10)}`}
                >
                  {ex}
                </button>
              ))}
            </div>
          )}
          <textarea
            ref={composer}
            className="composer-input"
            rows={1}
            placeholder={PLAIN_COPY.home.placeholder}
            aria-label="Prompt"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                if (!waiting) submit();
              }
            }}
            data-testid="inference-prompt"
            maxLength={500}
          />
          <div className="composer-bar">
            <label className="composer-tokens">
              <span>Max tokens</span>
              <input
                className="input input-mono"
                type="number"
                min={1}
                max={256}
                value={maxTokens}
                onChange={(e) => setMaxTokens(parseInt(e.target.value, 10) || 32)}
                data-testid="inference-max-tokens"
              />
            </label>
            <span className="composer-policy" data-testid="inference-model-policy">
              model identity: reported with response
            </span>
            <span className="composer-hint" aria-hidden="true">
              Enter to send · Shift+Enter for a new line
            </span>
            {waiting && (
              <button type="button" className="btn btn-ghost btn-sm" onClick={stopWaiting} data-testid="btn-stop-waiting">
                <CircleStop size={14} /> Stop waiting
              </button>
            )}
            <button className="btn btn-primary composer-send" onClick={submit} disabled={waiting || !draft.trim()} data-testid="btn-run-inference">
              {waiting ? (
                <>
                  <Loader2 size={15} className="spin" /> {PLAIN_COPY.ask.asking}
                </>
              ) : (
                <>
                  <Send size={15} /> {PLAIN_COPY.home.ask}
                </>
              )}
            </button>
          </div>
        </div>
        <details className="composer-note" data-testid="paid-mode-unavailable">
          <summary>
            <strong>Prompts are free; worker rewards are separate.</strong> <span className="composer-note-more">How that works</span>
          </summary>
          <p>
            This free prompt path does not sign or submit a paid requester escrow. A coordinator may still assign the prompt to an eligible community worker and
            return a validator-authorized <code>0x25</code> reward transaction for that worker. It is pending until the selected chain host reports a
            successful mined receipt; the person submitting the prompt is neither charged nor rewarded. VRF or replica selection alone is not payment
            approval.
          </p>
        </details>
      </div>
    </div>
  );
}

function EmptyThread() {
  return (
    <div className="chat-empty">
      <h2 className="chat-empty-title">{PLAIN_COPY.home.emptyTitle}</h2>
      <ul className="chat-empty-points">
        <li>
          <strong>Who served it.</strong> Your own node first; a named coordinator or community worker when yours can&rsquo;t.
        </li>
        <li>
          <strong>Whether it was checked.</strong> Marked checked only when another computer re-ran it and the agreement is authenticated.
        </li>
        <li>
          <strong>What reached the chain.</strong> A computation claim or a receipt, and never more than the chain reports.
        </li>
      </ul>
      <p className="chat-empty-note">Each prompt is answered on its own: earlier messages are not sent along as context.</p>
    </div>
  );
}

function useSeconds(since: number, running: boolean) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!running) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [running]);
  return Math.max(0, Math.floor((now - since) / 1000));
}

function Pending({ turn }: { turn: ChatTurn }) {
  const seconds = useSeconds(turn.sentAt, true);
  const where = turn.route ? ROUTE_WAITING[turn.route] : PLAIN_COPY.ask.sending;
  const slow = !turn.detached && seconds >= SLOW_AFTER_SECS;
  return (
    <div className="answer answer-pending" aria-busy="true" data-testid="inference-pending">
      <header className="answer-head">
        <span className="answer-mark" aria-hidden="true" />
        <span className="answer-by" data-testid="inference-pending-title">{turn.detached ? PLAIN_COPY.ask.stoppedWaiting : where}</span>
        <span className="answer-stats" data-testid="inference-pending-elapsed">{formatElapsed(seconds)}</span>
      </header>
      <StageTrack
        label="What has happened to this prompt so far"
        stages={[
          { name: "Sent", state: "done", detail: timeOf(turn.sentAt) },
          {
            name: "Served",
            state: "wait",
            detail: turn.detached ? "no answer yet" : turn.route ? `waiting for ${ROUTE_SHORT[turn.route]}` : "sending",
          },
          { name: "Checked", state: "open", detail: "reported with the answer" },
          { name: "On chain", state: "open", detail: "reported with the answer" },
        ]}
      />
      <p className="pending-note">
        {slow && <>{PLAIN_COPY.ask.slow} </>}
        {turn.detached ? PLAIN_COPY.ask.stoppedNote : PLAIN_COPY.ask.waitNote}
      </p>
    </div>
  );
}

const COPIED_RESET_MS = 2_500;

/**
 * A question that got no answer: one plain sentence, what to do, and the text
 * that came back behind Show details, selectable and copyable for a report.
 * Through v0.8.14 the raw error was the body of this card, in red.
 */
function Failed({ turn, latest, onRetry, disabled }: { turn: ChatTurn; latest: boolean; onRetry: () => void; disabled: boolean }) {
  const failure = describeFailure(turn.error);
  const tid = (name: string) => (latest ? name : `${name}-earlier`);
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState<"idle" | "copied" | "failed">("idle");
  const detailsId = `${turn.id}-details`;
  useEffect(() => {
    if (copied === "idle") return;
    const timer = window.setTimeout(() => setCopied("idle"), COPIED_RESET_MS);
    return () => window.clearTimeout(timer);
  }, [copied]);
  const copyDetails = () => {
    // Without a clipboard API the call throws before returning a promise.
    void Promise.resolve()
      .then(() => navigator.clipboard.writeText(failure.raw))
      .then(
        () => setCopied("copied"),
        () => setCopied("failed"),
      );
  };
  const waited = turn.settledAt ? formatElapsed(Math.max(0, Math.floor((turn.settledAt - turn.sentAt) / 1000))) : null;
  return (
    <div className="answer answer-failed" data-testid={tid("inference-error")} data-failure-kind={failure.kind}>
      <header className="answer-head">
        <span className="answer-mark" aria-hidden="true" />
        <span className="answer-by" data-testid={tid("inference-error-title")}>{failure.title}</span>
        {waited && <span className="answer-stats">{waited}</span>}
      </header>
      <p className="pending-note">{failure.note}</p>
      <div className="answer-tools">
        <button type="button" className="chip-btn" onClick={onRetry} disabled={disabled} data-testid={tid("inference-error-retry")}>
          {failure.retry}
        </button>
        {failure.raw && (
          <button
            type="button"
            className="chip-btn"
            onClick={() => setOpen((o) => !o)}
            aria-expanded={open}
            aria-controls={detailsId}
            data-testid={tid("inference-error-details-toggle")}
          >
            {open ? PLAIN_COPY.actions.hideDetails : PLAIN_COPY.actions.showDetails}
          </button>
        )}
        {failure.raw && (
          <button type="button" className="chip-btn" onClick={copyDetails} data-testid={tid("inference-error-copy")}>
            {copied === "copied" ? <Check size={13} /> : <Copy size={13} />}{" "}
            {copied === "copied" ? PLAIN_COPY.actions.copied : copied === "failed" ? PLAIN_COPY.actions.copyFailed : PLAIN_COPY.actions.copyDetails}
          </button>
        )}
      </div>
      {/* Rendered while hidden so aria-controls always names a real element. */}
      {failure.raw && (
        <pre id={detailsId} className="answer-details" hidden={!open} data-testid={tid("inference-error-details")}>
          {failure.raw}
        </pre>
      )}
    </div>
  );
}
