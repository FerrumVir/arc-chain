// How a community node's connection reads on the Dashboard.
//
// Live mode (window.__ARC_LIVE__ = 9090) makes tauri.ts hit fetch instead of
// its in-process mock. The configured coordinators use publicly trusted
// direct-IP HTTPS origins, so these routes mirror the origins a shipped v0.8
// desktop actually probes.
//
// A community node never has peers: arc-net admits only the validators. Its
// healthy steady state is peers 0, height 0, a "degraded" local /health with
// chain participation off, and a coordinator answering. Through v0.8.14 that
// state showed three banners: "HTTP client mode", "Client mode" with a "Reset
// peer state" button that could not help (there is no peer cache to repair
// when there are no peers to be had), and "Connecting to peers" counting up.

import { expect, test, type Page } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { seedOnboarded } from "./helpers";

// Operator vocabulary that must not reach the screen in a normal state.
const JARGON =
  /client mode|lite|syncing|stale peer|rebootstrap|QUIC|handshak|data centers|chain participation|degraded|cannot receive/i;

// The local /health of a healthy community node, as arc-node reports it today.
const HEALTHY_COMMUNITY = {
  status: "degraded",
  version: "0.8.16",
  height: 0,
  peers: 0,
  uptime_secs: 60,
  dag_round: 0,
  dag_committed: 0,
  validators: 0,
  chain_participation_enabled: false,
};

/** The local node answers `local` (or nothing), and one coordinator answers, or none. */
async function liveNode(
  page: Page,
  local: Record<string, unknown>,
  coordinatorHost: string | null,
) {
  await seedOnboarded(page);
  await page.addInitScript(() => {
    (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = 9090;
  });
  await page.route("**/health", (route) => {
    const host = new URL(route.request().url()).hostname;
    if (host === "127.0.0.1") {
      return route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify(local),
      });
    }
    if (coordinatorHost !== null && host === coordinatorHost) {
      return route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ status: "ok" }),
      });
    }
    return route.fulfill({ status: 503, body: "unavailable" });
  });
}

test.describe("Dashboard: a community node's connection", () => {
  test("0 peers, a degraded health and chain participation off read as connected when a coordinator answers", async ({
    page,
  }) => {
    await liveNode(page, HEALTHY_COMMUNITY, "149.28.32.76");
    await page.goto("/");

    const notice = page.getByTestId("connection-notice");
    await expect(notice).toBeVisible({ timeout: 15_000 });
    await expect(notice).toHaveAttribute("data-state", "connected");
    await expect(notice).toHaveAttribute("role", "status");
    // ARC's own probe reached the coordinator; the sentence does not claim the node did.
    await expect(notice).toContainText("ARC is connected to the network through NYC");
    await expect(notice).not.toContainText("node is connected");
    await expect(notice).toContainText("0 peers is normal");
    await expect(notice).not.toHaveText(JARGON);
    // Browser live mode reports no pid, so the Dashboard reads this node as managed outside the app and its
    // subtitle says so; dashboard.spec.ts pins the connected / connecting subtitles for a node the app spawned.
    await expect(page.getByTestId("dashboard-subtitle")).not.toHaveText(JARGON);

    // The pill, the sidebar and the node card agree, in the same plain word.
    await expect(page.getByTestId("titlebar").getByTestId("status-pill-live")).toHaveText(
      PLAIN_COPY.connection.labels.connected,
    );
    await expect(page.getByTestId("sidebar-connection")).toContainText("via NYC");
    await expect(page.getByTestId("sidebar-connection")).not.toContainText("0 peers");

    // Nothing on the screen is an alert, and the peer-reset button that could not help is gone.
    await expect(page.getByTestId("dashboard").getByRole("alert")).toHaveCount(0);
    await expect(page.getByTestId("reset-peer-state-btn")).toHaveCount(0);
  });

  test("an older node that does not report chain participation reads the same way", async ({
    page,
  }) => {
    const older: Record<string, unknown> = { ...HEALTHY_COMMUNITY, status: "ok" };
    delete older.chain_participation_enabled;
    await liveNode(page, older, "140.82.16.112");
    await page.goto("/");

    const notice = page.getByTestId("connection-notice");
    await expect(notice).toBeVisible({ timeout: 15_000 });
    await expect(notice).toHaveAttribute("data-state", "connected");
    await expect(notice).toContainText("through LAX");
    await expect(notice).not.toHaveText(JARGON);
    await expect(page.getByTestId("reset-peer-state-btn")).toHaveCount(0);
  });

  test("a running node that nothing has answered yet is connecting, not broken", async ({
    page,
  }) => {
    await liveNode(page, { ...HEALTHY_COMMUNITY, status: "ok" }, null);
    await page.goto("/");

    const notice = page.getByTestId("connection-notice");
    await expect(notice).toBeVisible({ timeout: 15_000 });
    await expect(notice).toHaveAttribute("data-state", "connecting");
    await expect(notice).toHaveAttribute("role", "status");
    await expect(notice).toContainText("Connecting to the ARC network");
    await expect(notice).toContainText(PLAIN_COPY.connection.connecting);
    await expect(notice).not.toHaveText(JARGON);
    await expect(page.getByTestId("dashboard-subtitle")).not.toHaveText(JARGON);
    await expect(page.getByTestId("titlebar").getByTestId("status-pill-syncing")).toHaveText(
      PLAIN_COPY.connection.labels.connecting,
    );
    await expect(page.getByTestId("dashboard").getByRole("alert")).toHaveCount(0);
    await expect(page.getByTestId("reset-peer-state-btn")).toHaveCount(0);
  });
});
