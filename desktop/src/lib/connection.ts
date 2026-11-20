import { hostLabel } from "./hosts";
import { PLAIN_COPY } from "./plain-copy";
import type { NodeStatus } from "./types";

export type ConnectionState =
  | "checking"
  | "starting"
  | "off"
  | "unreachable"
  | "connecting"
  | "connected";

export type ConnectionLevel = "checking" | "off" | "offline" | "syncing" | "live";

export const CONNECTING_LONG_SECS = 45;

export interface ConnectionView {
  state: ConnectionState;
  label: string;
  level: ConnectionLevel;
  via: string | null;
  sentence: string;
}

export type ConnectionInput = Pick<NodeStatus, "running" | "pid" | "health"> &
  Partial<Pick<NodeStatus, "peers" | "coordinatorUrl">>;

function peersText(peers: number): string {
  return `${peers} ${peers === 1 ? "peer" : "peers"}`;
}

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

export function isUnreachableMessage(lastError: string | null | undefined): boolean {
  return !!lastError && lastError.startsWith("No node is answering on");
}
