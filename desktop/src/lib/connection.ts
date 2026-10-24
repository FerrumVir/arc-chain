import { hostLabel } from "./hosts";
import { PLAIN_COPY } from "./plain-copy";
import type { NodeStatus } from "./types";

/**
 * How the node reaches the network, in the words every screen uses.
 *
 * The native status carries a health level written for operators: `live`
 * (peers), `lite` (no peers, a coordinator answered), `syncing` (running,
 * nothing answered yet) and `offline` (nothing at all). A community node never
 * has peers: arc-net admits only the validators, so its steady state is zero
 * peers, height zero and a coordinator answering. Through v0.8.14 that steady
 * state read as "Lite", "Syncing", "0 peers" and a client-mode banner, which
 * looked like a fault. Here it reads as what it is: connected.
 */
export type ConnectionState =
  /** No status has arrived yet. */
  | "checking"
  /** The process is up and its RPC is not answering yet (loading the model). */
  | "starting"
  /** Not running, and the network is reachable or unknown. */
  | "off"
  /** Not running, and no coordinator answered: this computer is offline. */
  | "unreachable"
  /** Running; no peer and no coordinator has answered yet. */
  | "connecting"
  /** Running; a peer or a coordinator answered. */
  | "connected";

/** The dot and pill colour classes (styles/app.css). */
export type ConnectionLevel = "checking" | "off" | "offline" | "syncing" | "live";

/**
 * A running node that nothing has answered for this long is told, on the
 * Dashboard and on Home alike, to check the internet connection. Calm, no
 * red: ARC keeps trying by itself.
 */
export const CONNECTING_LONG_SECS = 45;

export interface ConnectionView {
  state: ConnectionState;
  /** One or two words, for the pill and the sidebar chip. */
  label: string;
  level: ConnectionLevel;
  /** How a running node reaches the network: "8 peers", "via LAX", or the connecting ellipsis. */
  via: string | null;
  /** One plain sentence. */
  sentence: string;
}

export type ConnectionInput = Pick<NodeStatus, "running" | "pid" | "health"> &
  Partial<Pick<NodeStatus, "peers" | "coordinatorUrl">>;

function peersText(peers: number): string {
  return `${peers} ${peers === 1 ? "peer" : "peers"}`;
}

/** The connection a status describes; `undefined` is a status that has not arrived. */
export function describeConnection(status: ConnectionInput | undefined): ConnectionView {
  const copy = PLAIN_COPY.connection;
  if (!status) {
    return { state: "checking", label: copy.labels.checking, level: "checking", via: null, sentence: copy.checking };
  }
  if (status.running) {
    const peers = status.peers ?? 0;
    if (status.health === "live" && peers >= 1) {
      const via = peersText(peers);
      return {
        state: "connected",
        label: copy.labels.connected,
        level: "live",
        via: copy.viaPeers(via),
        sentence: copy.connectedPeers(via),
      };
    }
    if (status.health === "lite" || status.coordinatorUrl) {
      // "lite" without an origin cannot happen on the native side; name what is known.
      const named = status.coordinatorUrl ? hostLabel(status.coordinatorUrl) : "a coordinator";
      return {
        state: "connected",
        label: copy.labels.connected,
        level: "live",
        via: copy.viaCoordinator(named),
        sentence: copy.connectedCoordinator(named),
      };
    }
    if (status.health === "live") {
      // Live without a peer count: connected, by the native side's own verdict.
      return { state: "connected", label: copy.labels.connected, level: "live", via: null, sentence: copy.connected };
    }
    return { state: "connecting", label: copy.labels.connecting, level: "syncing", via: copy.viaConnecting, sentence: copy.connecting };
  }
  if (status.pid != null) {
    return { state: "starting", label: copy.labels.starting, level: "checking", via: null, sentence: copy.starting };
  }
  if (status.health === "offline") {
    return { state: "unreachable", label: copy.labels.unreachable, level: "offline", via: null, sentence: copy.unreachable };
  }
  return { state: "off", label: copy.labels.off, level: "off", via: null, sentence: copy.off };
}

/**
 * The native status's own words for "nothing answered": rpc_client.rs writes
 * this into `lastError` when the node is off and no public seed is reachable.
 * It describes this computer's connection, not a failed start.
 */
export function isUnreachableMessage(lastError: string | null | undefined): boolean {
  return !!lastError && lastError.startsWith("No node is answering on");
}
