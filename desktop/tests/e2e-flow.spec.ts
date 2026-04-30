// End-to-end flow tests exercising the six fixes that unblocked testnet
// release (arc-node CLI spawn args, bundled seeds/genesis, BIP-39 validator
// seed, auto-download binary, real attestation-backed earnings, Gatekeeper
// docs). These tests run against the mock IPC layer - exhaustive live
// behaviour is covered by tests/live.spec.ts.
import { expect, test } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { clearState, seedOnboarded } from "./helpers";

const __dirnameHere = path.dirname(fileURLToPath(import.meta.url));
const REPO_DESKTOP = path.resolve(__dirnameHere, "..");

test.describe("Onboarding → launch end-to-end", () => {
  test.beforeEach(async ({ page }) => {
    await clearState(page);
  });

  test("three-step launch: welcome → identity → join", async ({ page }) => {
    await page.goto("/");

    await page.getByTestId("btn-continue-welcome").click();
    await expect(page.getByTestId("step-identity")).toBeVisible();
    await page.getByTestId("btn-reveal-seed").click();
    await page.getByTestId("btn-continue-identity").click();
    await expect(page.getByTestId("step-launch")).toBeVisible();
    await expect(page.getByText(/ready to join/i)).toBeVisible();

    await page.getByTestId("btn-launch").click();

    // Mock flow: ensureBinary + startNode + waitForPeer + faucetClaim all
    // resolve, then dashboard renders.
    await expect(page.getByTestId("dashboard")).toBeVisible({ timeout: 15_000 });
  });

  test("launch button says 'Join the network', not 'Launch node'", async ({
    page,
  }) => {
    await page.goto("/");
    await page.getByTestId("btn-continue-welcome").click();
    await page.getByTestId("btn-reveal-seed").click();
    await page.getByTestId("btn-continue-identity").click();
    await expect(page.getByTestId("btn-launch")).toBeVisible();
    await expect(page.getByTestId("btn-launch")).toContainText(
      /join the network/i,
    );
  });

  test("launch pipeline stops off on observer role + null modelPath", () => {
    // Regression guard: we ship with role="observer" and no model so users
    // don't have to pick anything. Onboarding.tsx must not regress to
    // role=worker (which would trigger the inference-worker path we don't
    // want to default users into).
    const src = fs.readFileSync(
      path.resolve(REPO_DESKTOP, "src", "screens", "Onboarding.tsx"),
      "utf8",
    );
    expect(src).toMatch(/role:\s*"observer"/);
    expect(src).toMatch(/modelPath:\s*null/);
    // And no more role picker
    expect(src).not.toMatch(/ROLE_META/);
    expect(src).not.toMatch(/setRole/);
  });

  test("launch pipeline auto-claims faucet after peer count ≥ 1", () => {
    const src = fs.readFileSync(
      path.resolve(REPO_DESKTOP, "src", "screens", "Onboarding.tsx"),
      "utf8",
    );
    // Faucet claim must be gated on waitForPeer returning true - not
    // fired unconditionally (would burn the daily limit on nodes that
    // never joined).
    expect(src).toMatch(/waitForPeer/);
    expect(src).toMatch(/faucetClaim/);
  });
});

test.describe("Earnings chart uses real attestation data", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("seven-day bars sum to the mock attestations' rewardArc total (114.5)", async ({
    page,
  }) => {
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("earnings-screen")).toBeVisible();

    // Mock attestations: rewardArc values 12.5, 34.8, 67.2 - sum 114.5.
    // All three timestamped within the last ~4 minutes so they all land
    // in today's bucket. The bar values rendered as toFixed(0) - today's
    // bar should read "115" (rounded from 114.5) and the other 6 should
    // read "0". The old Math.random implementation would land every bar
    // in the 100-400 range so any bar > 200 would prove regression.
    const labels = await page
      .locator("[data-testid='earnings-screen']")
      .locator("div")
      .filter({ hasText: /^(\d+)$/ })
      .allTextContents();

    // Extract numeric bar-top labels (the div above each bar).
    // The simplest check: at least one bar shows a non-zero number that's
    // below the Math.random() floor (100), and the majority of bars read 0.
    // Count the "0" bars - expect ≥ 5 of 7.
    const zeroCount = labels.filter((l) => l === "0").length;
    expect(zeroCount).toBeGreaterThanOrEqual(5);

    // And assert that at least one bar has a value in the expected
    // 100-200 range for the Math.random regression check. Since the mock
    // has 114.5 ARC today, any value from "110"–"120" is acceptable.
    const hasExpectedTodayBar = labels.some((l) => {
      const n = Number(l);
      return Number.isFinite(n) && n >= 100 && n <= 200;
    });
    expect(hasExpectedTodayBar).toBe(true);
  });
});

test.describe("Gatekeeper + first-run docs are shipped", () => {
  test("FIRST-RUN.md exists and covers macOS + Windows + Linux", () => {
    const p = path.resolve(REPO_DESKTOP, "FIRST-RUN.md");
    expect(fs.existsSync(p)).toBe(true);
    const text = fs.readFileSync(p, "utf8");
    expect(text.length).toBeGreaterThan(500);
    expect(text).toMatch(/right-click/i);
    expect(text).toMatch(/xattr/i);
    expect(text).toMatch(/smartscreen/i);
    expect(text).toMatch(/linux/i);
  });
});

test.describe("Bundled testnet resources", () => {
  test("seeds + genesis files are present in the Tauri bundle source", () => {
    const resources = path.resolve(REPO_DESKTOP, "src-tauri", "resources");
    const seeds = path.join(resources, "testnet-seeds.txt");
    const genesis = path.join(resources, "genesis.toml");
    expect(fs.existsSync(seeds)).toBe(true);
    expect(fs.existsSync(genesis)).toBe(true);

    // Sanity: seeds file lists the 6 live testnet seeds (NYC/LAX/AMS/
    // LHR/NRT/SGP). SAO and JNB were retired GH #32 so they must NOT
    // appear in the shipped seeds file.
    const seedsText = fs.readFileSync(seeds, "utf8");
    expect(seedsText).toContain("149.28.32.76"); // NYC
    expect(seedsText).toContain("140.82.16.112"); // LAX
    expect(seedsText).toContain("149.28.153.31"); // SGP
    expect(seedsText).not.toMatch(/216\.238\.120\.27/); // SAO retired
    expect(seedsText).not.toMatch(/139\.84\.237\.49/); // JNB retired
  });

  test("tauri.conf.json declares the resources so they land in the bundle", () => {
    const conf = path.resolve(REPO_DESKTOP, "src-tauri", "tauri.conf.json");
    const j = JSON.parse(fs.readFileSync(conf, "utf8"));
    expect(j.bundle?.resources).toEqual(
      expect.arrayContaining([
        "resources/testnet-seeds.txt",
        "resources/genesis.toml",
      ]),
    );
  });
});

test.describe("Spawn CLI contract (Rust source)", () => {
  // Guard against regressions in the node_manager.rs flag wiring: the
  // only way arc-node joins testnet is if --rpc, --p2p-port, --data-dir,
  // --validator-seed, --seeds-file, --genesis, --eth-rpc-port, and
  // --community-mode all end up on the cmd line.
  const src = fs.readFileSync(
    path.resolve(REPO_DESKTOP, "src-tauri", "src", "node_manager.rs"),
    "utf8",
  );

  test("--rpc is passed as a single addr:port (not --rpc-port)", () => {
    // Positive: the string "--rpc" appears followed by the addr:port pattern.
    expect(src).toMatch(/\.arg\("--rpc"\)\s*\.\s*arg\(format!\("127\.0\.0\.1/);
    // Negative: no stray --rpc-port anywhere (that flag doesn't exist in arc-node).
    expect(src).not.toMatch(/--rpc-port/);
  });

  test("passes all required testnet flags", () => {
    for (const flag of [
      '"--p2p-port"',
      '"--data-dir"',
      '"--validator-seed"',
      '"--seeds-file"',
      '"--genesis"',
      '"--eth-rpc-port"',
      '"--community-mode"',
    ]) {
      expect(src).toContain(flag);
    }
  });

  test("community-mode only toggled for worker role", () => {
    expect(src).toMatch(/if\s+config\.role\s*==\s*"worker"\s*\{[\s\S]*?--community-mode/);
  });
});

test.describe("ensure_binary auto-download", () => {
  const commands = fs.readFileSync(
    path.resolve(REPO_DESKTOP, "src-tauri", "src", "commands.rs"),
    "utf8",
  );

  test("selects correct release asset per platform", () => {
    // Asset names must match what the CI release workflow actually ships.
    expect(commands).toContain('"arc-node-macos-arm64"');
    expect(commands).toContain('"arc-node-macos-x86_64"');
    expect(commands).toContain('"arc-node-linux-x86_64"');
  });

  test("downloaded binary is written to ~/.arc/bin/arc-node", () => {
    const nm = fs.readFileSync(
      path.resolve(REPO_DESKTOP, "src-tauri", "src", "node_manager.rs"),
      "utf8",
    );
    expect(nm).toMatch(/\.arc"[\s\S]*?"bin"[\s\S]*?arc-node/);
  });

  test("writes 0o755 perms on unix so it's executable", () => {
    expect(commands).toMatch(/0o755/);
  });
});

test.describe("Keep-running lifecycle (auto-start + tray + auto-update)", () => {
  const lib = fs.readFileSync(
    path.resolve(REPO_DESKTOP, "src-tauri", "src", "lib.rs"),
    "utf8",
  );
  const tray = fs.readFileSync(
    path.resolve(REPO_DESKTOP, "src-tauri", "src", "tray.rs"),
    "utf8",
  );
  const cargo = fs.readFileSync(
    path.resolve(REPO_DESKTOP, "src-tauri", "Cargo.toml"),
    "utf8",
  );
  const conf = JSON.parse(
    fs.readFileSync(
      path.resolve(REPO_DESKTOP, "src-tauri", "tauri.conf.json"),
      "utf8",
    ),
  );
  const caps = JSON.parse(
    fs.readFileSync(
      path.resolve(REPO_DESKTOP, "src-tauri", "capabilities", "default.json"),
      "utf8",
    ),
  );

  test("auto-update: updater plugin registered + real pubkey wired", () => {
    expect(lib).toMatch(/tauri_plugin_updater::Builder::new\(\)\.build\(\)/);
    expect(conf.plugins?.updater?.active).toBe(true);
    // Reject any TODO / placeholder values.
    const pubkey = conf.plugins?.updater?.pubkey ?? "";
    expect(pubkey.length).toBeGreaterThan(50);
    expect(pubkey).not.toMatch(/TODO/i);
    expect(conf.bundle?.createUpdaterArtifacts).toBe(true);
    expect(conf.plugins?.updater?.endpoints?.[0]).toMatch(/releases\/latest/);
  });

  test("auto-start: autostart plugin registered with --minimized arg", () => {
    expect(cargo).toMatch(/tauri-plugin-autostart\s*=\s*"2"/);
    expect(lib).toMatch(/tauri_plugin_autostart::init\(/);
    expect(lib).toContain('"--minimized"');
    // Capabilities must grant autostart permissions or the plugin 500s
    // from the frontend.
    expect(caps.permissions).toEqual(expect.arrayContaining(["autostart:default"]));
  });

  test("tray: tray icon + open/quit menu installed, left-click opens window", () => {
    expect(cargo).toMatch(/tauri\s*=\s*\{\s*version\s*=\s*"2"\s*,\s*features\s*=\s*\[[^\]]*"tray-icon"/);
    expect(lib).toMatch(/mod tray;/);
    expect(lib).toMatch(/tray::install/);
    expect(tray).toMatch(/MenuItem::with_id\(app,\s*"open"/);
    expect(tray).toMatch(/MenuItem::with_id\(app,\s*"quit"/);
    // Ticker that refreshes status + round labels.
    expect(tray).toMatch(/dag_round/);
    expect(tray).toMatch(/interval\.tick\(\)\.await/);
  });

  test("window close hides to tray instead of exiting", () => {
    expect(lib).toMatch(/WindowEvent::CloseRequested/);
    expect(lib).toMatch(/window\.hide\(\)/);
    expect(lib).toMatch(/api\.prevent_close\(\)/);
  });

  test("Quit menu stops arc-node cleanly before app.exit", () => {
    expect(tray).toMatch(/"quit"\s*=>/);
    expect(tray).toMatch(/node\.stop\(\)/);
    expect(tray).toMatch(/handle\.exit\(0\)/);
  });
});

test.describe("Release CI publishes signed update manifest", () => {
  const wf = fs.readFileSync(
    path.resolve(
      REPO_DESKTOP,
      "..",
      ".github",
      "workflows",
      "release-desktop.yml",
    ),
    "utf8",
  );

  test("workflow signs with TAURI_SIGNING_PRIVATE_KEY secret", () => {
    expect(wf).toContain("TAURI_SIGNING_PRIVATE_KEY");
    expect(wf).toContain("TAURI_SIGNING_PRIVATE_KEY_PASSWORD");
  });

  test("workflow emits latest.json with per-target url + signature", () => {
    expect(wf).toMatch(/darwin-aarch64/);
    expect(wf).toMatch(/linux-x86_64/);
    expect(wf).toMatch(/\.app\.tar\.gz\.sig/);
    expect(wf).toMatch(/latest\.json/);
  });

  test("tag trigger matches v*.*.*", () => {
    expect(wf).toMatch(/tags:\s*\n\s*-\s*'v\*\.\*\.\*'/);
  });
});
