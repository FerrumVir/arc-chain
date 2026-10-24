import { expect, test } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { describeConnection, isUnreachableMessage } from "../src/lib/connection";

// Pure logic: no page. How the node's connection reads on every screen (the
// titlebar pill, the sidebar chip, the Dashboard line, the strip on Home).
//
// A community node never has peers: arc-net admits only the validators. Its
// healthy steady state is peers 0, height 0 and a coordinator answering, which
// the native status reports as health "lite". Through v0.8.14 that read as
// "Lite", "Running · 0 peers" and a client-mode banner.

const __dirnameHere = path.dirname(fileURLToPath(import.meta.url));

const community = {
  running: true,
  pid: 4242,
  health: "lite" as const,
  peers: 0,
  coordinatorUrl: "https://140.82.16.112",
};

test.describe("connection in plain words", () => {
  test("a community node with 0 peers and a coordinator answering is connected", () => {
    const view = describeConnection(community);
    expect(view.state).toBe("connected");
    expect(view.label).toBe(PLAIN_COPY.connection.labels.connected);
    expect(view.level).toBe("live");
    expect(view.via).toBe("via LAX");
    // The desktop's own probe reached the coordinator: ARC is connected. An observer node never talks to a
    // coordinator, so the sentence does not say the node is.
    expect(view.sentence).toContain("ARC is connected to the network through LAX");
    expect(view.sentence).not.toContain("node is connected");
    expect(view.sentence).toContain("0 peers is normal");
    // The native side names the origin whenever it reports "lite"; without one, say what is known.
    expect(describeConnection({ ...community, coordinatorUrl: null }).via).toBe("via a coordinator");
  });

  test("peers read as peers; nothing answering reads as connecting, not as a fault", () => {
    const peers = describeConnection({ ...community, health: "live", peers: 5, coordinatorUrl: null });
    expect(peers).toMatchObject({ state: "connected", via: "5 peers", level: "live" });
    expect(peers.sentence).toBe(PLAIN_COPY.connection.connectedPeers("5 peers"));
    expect(describeConnection({ ...community, health: "live", peers: 1, coordinatorUrl: null }).via).toBe("1 peer");
    const connecting = describeConnection({ ...community, health: "syncing", coordinatorUrl: null });
    expect(connecting).toMatchObject({
      state: "connecting",
      label: PLAIN_COPY.connection.labels.connecting,
      level: "syncing",
      via: PLAIN_COPY.connection.viaConnecting,
    });
    expect(connecting.sentence).toBe(PLAIN_COPY.connection.connecting);
  });

  test("off is off, and only nothing reachable at all is a missing connection", () => {
    expect(describeConnection(undefined)).toMatchObject({ state: "checking", level: "checking" });
    expect(describeConnection({ running: false, pid: 4242, health: "offline" })).toMatchObject({
      state: "starting",
      label: PLAIN_COPY.connection.labels.starting,
      level: "checking",
    });
    // The native side still probes the seeds while the node is off; one answering is "lite".
    expect(
      describeConnection({ running: false, pid: null, health: "lite", coordinatorUrl: "https://149.28.32.76" }),
    ).toMatchObject({ state: "off", label: PLAIN_COPY.connection.labels.off, level: "off" });
    const unreachable = describeConnection({ running: false, pid: null, health: "offline" });
    expect(unreachable).toMatchObject({
      state: "unreachable",
      label: PLAIN_COPY.connection.labels.unreachable,
      level: "offline",
    });
    expect(unreachable.sentence).toContain("internet connection");
  });

  test("no label is operator vocabulary, and the native 'nothing answered' text is a connection, not a failed start", () => {
    for (const label of Object.values(PLAIN_COPY.connection.labels)) {
      expect(label).not.toMatch(/lite|sync|offline|degraded|peer|rpc|quic|client mode/i);
    }
    const native =
      "No node is answering on http://127.0.0.1:9090 and every public seed is unreachable. Start your node, or check your internet connection and firewall.";
    expect(isUnreachableMessage(native)).toBe(true);
    expect(isUnreachableMessage("arc-node exited unexpectedly (code 1)")).toBe(false);
    expect(isUnreachableMessage(null)).toBe(false);
    // The wording it recognises is the wording rpc_client.rs writes.
    const rpcClient = fs.readFileSync(
      path.resolve(__dirnameHere, "..", "src-tauri", "src", "rpc_client.rs"),
      "utf8",
    );
    expect(rpcClient).toContain('"No node is answering on {} and every public seed is unreachable.');
  });
});
