import { Loader2, SquarePen, Send, CircleStop } from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { InfoPopover } from "../components/InfoPopover";
import { NativePaidRequests } from "../components/NativePaidRequests";
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

// "direct" is also the route when the app skips its own node on purpose: the
// session reads the chain from a remote validator, so the native local route
// refuses before sending (commands.rs run_inference_local_inner). It must not
// say the node was asked, or claim a time the code does not promise.
const ROUTE_WAITING: Record<DispatchRoute, string> = {
  local: "Waiting for your node",
  direct: "Waiting for the ARC network",
  consensus: "Waiting for a network consensus run",
};
const ROUTE_SHORT: Record<DispatchRoute, string> = { local: "your node", direct: "the ARC network", consensus: "network consensus" };
const NETWORK_WAIT =
  "Your prompt goes to the ARC network, where a community worker normally answers it and validators check the answer. This can take several minutes.";

/** One short line for screen readers about the newest turn, instead of reading whole answers aloud. */
function announce(turns: ChatTurn[]): string {
  const t = turns[turns.length - 1];
  if (!t) return "";
  if (t.status === "pending") return t.detached ? "Stopped waiting. A late answer will still appear." : t.route ? `${ROUTE_WAITING[t.route]}.` : "Sending.";
  if (t.status === "failed") return "The request did not complete.";
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
          <h1 className="page-title">Inference</h1>
          <p className="page-subtitle">Every answer shows who served it, whether a second computer checked it, and what reached the chain.</p>
        </div>
        <div className="chat-header-actions">
          <InfoPopover title="How this works">
            <p>
              Your prompt goes to the ARC validator this app reads the chain from (one for the whole session), at its <code>/inference/run</code>{" "}
              route. That validator gives it to a community worker when one is free and has validators check the answer before it comes back; with no
              worker free, the validators compute it themselves. This can take several minutes. Your own node takes the prompt only when it is the
              app&rsquo;s chain source, or when no validator could be reached. Either way the answer says where the compute ran and what the
              coordinator actually verified.
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

      <div className="composer" role="group" aria-label="Ask the network">
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
            placeholder="Ask the network anything…"
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
                  <Loader2 size={15} className="spin" /> Computing…
                </>
              ) : (
                <>
                  <Send size={15} /> Run inference
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
      <h2 className="chat-empty-title">Ask the network.</h2>
      <ul className="chat-empty-points">
        <li>
          <strong>Who served it.</strong> A named community worker or validator on the ARC network, or your own node when it is the app&rsquo;s chain
          source.
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
  const where = turn.route ? ROUTE_WAITING[turn.route] : "Sending";
  return (
    <div className="answer answer-pending" aria-busy="true" data-testid="inference-pending">
      <header className="answer-head">
        <span className="answer-mark" aria-hidden="true" />
        <span className="answer-by">{turn.detached ? "You stopped waiting" : where}</span>
        <span className="answer-stats">{seconds} s</span>
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
        {turn.detached
          ? "The request was already sent and can't be recalled. If an answer still arrives, it will appear here."
          : `${turn.route === "direct" ? `${NETWORK_WAIT} ` : ""}A sent request can't be recalled. You can stop waiting (below); if an answer still arrives, it will appear here.`}
      </p>
    </div>
  );
}

function Failed({ turn, latest, onRetry, disabled }: { turn: ChatTurn; latest: boolean; onRetry: () => void; disabled: boolean }) {
  const message = turn.error ?? "The request failed.";
  const nothingSent = message.startsWith(INFERENCE_PRE_DISPATCH_UNAVAILABLE);
  const mayStillRun = /OUTCOME_AMBIGUOUS|may still settle/i.test(message);
  return (
    <div className="answer answer-failed">
      <header className="answer-head">
        <span className="answer-mark" aria-hidden="true" />
        <span className="answer-by">{nothingSent ? "Not sent: no route could take it" : mayStillRun ? "No answer, and the outcome is unknown" : "Did not complete"}</span>
      </header>
      <div className="answer-error" data-testid={latest ? "inference-error" : "inference-error-earlier"}>
        {message}
      </div>
      <p className="pending-note">
        {nothingSent
          ? "Nothing was sent, so trying again is safe."
          : mayStillRun
            ? "The request may still be processed. Sending it again starts a second job."
            : "Check your node and the network status, then try again."}
      </p>
      <div className="answer-tools">
        <button type="button" className="chip-btn" onClick={onRetry} disabled={disabled}>
          {mayStillRun ? "Send again anyway" : "Try again"}
        </button>
      </div>
    </div>
  );
}
