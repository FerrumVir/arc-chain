// End-to-end flow tests exercising the six fixes that unblocked testnet
// release (arc-node CLI spawn args, bundled seeds/genesis, BIP-39 validator
// seed, auto-download binary, real attestation-backed earnings, Gatekeeper
// docs). These tests run against the mock IPC layer - exhaustive live
// behaviour is covered by tests/live.spec.ts.
import { expect, test } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import {
  clearState,
  seedOnboarded,
  seedOnboardedLegacy,
  walkToLaunch,
} from "./helpers";

const __dirnameHere = path.dirname(fileURLToPath(import.meta.url));
const REPO_DESKTOP = path.resolve(__dirnameHere, "..");

test.describe("Onboarding → launch end-to-end", () => {
  test.beforeEach(async ({ page }) => {
    await clearState(page);
  });

  test("four-step launch: welcome → identity → model → join", async ({
    page,
  }) => {
    await page.goto("/");

    await page.getByTestId("btn-continue-welcome").click();
    await expect(page.getByTestId("step-identity")).toBeVisible();
    await page.getByTestId("btn-reveal-seed").click();
    await page.getByTestId("btn-continue-identity").click();
    // Model picker (v0.6.0). This spec used to assert step-launch here.
    await expect(page.getByTestId("step-model")).toBeVisible();
    await page.getByTestId("tier-skip").click();
    await page.getByTestId("btn-continue-model").click();
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
    await walkToLaunch(page);
    await expect(page.getByTestId("btn-launch")).toBeVisible();
    await expect(page.getByTestId("btn-launch")).toContainText(
      /join the network/i,
    );
  });

  test("node role is derived from whether a model was downloaded", () => {
    // This previously asserted a literal `role: "observer"` + `modelPath:
    // null`, from the v0.5.x design where onboarding always shipped an
    // observer and the user picked nothing. v0.6.0 deliberately replaced
    // that: the user chooses a model tier, and the role is a CONSEQUENCE of
    // whether a model landed on disk. That is the behaviour to guard now -
    // the old assertion would force a regression back to a node that can
    // never earn.
    const src = fs.readFileSync(
      path.resolve(REPO_DESKTOP, "src", "screens", "Onboarding.tsx"),
      "utf8",
    );
    // Role is computed from modelPath, and observer is still the no-model
    // outcome.
    expect(src).toMatch(/role:\s*modelPath\s*\?\s*"worker"\s*:\s*"observer"/);
    // Skipping the model picker must still yield a null modelPath.
    expect(src).toMatch(/let modelPath: string \| null = null/);
    // And still no role picker - that part of the old contract stands.
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

  test("seven-day bars count only the user's own dated attestations", async ({
    page,
  }) => {
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("earnings-screen")).toBeVisible();

    // The mock has three attestations: two credited to this user at
    // 2.5 ARC each with real timestamps, and one from another validator
    // with no timestamp and no reward. Only the first two are bucketable,
    // so today's bar is 5 ARC and the other six are 0.
    //
    // The previous version of this test expected 114.5 across bars, from
    // per-attestation rewards of 12.5/34.8/67.2 - figures the chain never
    // produced. The real testnet rate is a flat 2.5 per attestation.
    const chart = page.getByTestId("weekly-chart");
    await expect(chart).toBeVisible();

    const labels = await chart
      .locator("div")
      .filter({ hasText: /^(\d+)$/ })
      .allTextContents();

    const zeroCount = labels.filter((l) => l === "0").length;
    expect(zeroCount).toBeGreaterThanOrEqual(6);

    // Today's bar: 2 × 2.5 = 5, rendered with toFixed(0).
    expect(labels).toContain("5");

    // The other validator's attestation must not have been counted.
    expect(labels.some((l) => Number(l) > 5)).toBe(false);
  });

  test("another validator's attestation is not shown as the user's income", async ({
    page,
  }) => {
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    const feed = page.getByTestId("all-attestations");
    await expect(feed).toBeVisible();
    // Two of four mock attestations are the user's; the other two (another
    // validator's row and an old-seed padding row) must not be counted.
    await expect(page.getByText(/2 yours · 4 shown/)).toBeVisible();
    await expect(feed.getByText("network", { exact: true })).toHaveCount(2);
  });
});

test.describe("Recovery phrase never reaches localStorage", () => {
  test("a phrase persisted by an older build is scrubbed on load", async ({
    page,
  }) => {
    // Older builds wrote the full identity - seedPhrase included - into
    // localStorage in plaintext. Those blobs are still on disk, so the
    // store scrubs them on load rather than only avoiding new writes.
    await seedOnboardedLegacy(page);
    await page.goto("/");
    await expect(page.getByTestId("dashboard")).toBeVisible();

    const stored = await page.evaluate(() =>
      localStorage.getItem("arc-desktop-state-v1"),
    );
    expect(stored).toBeTruthy();
    expect(stored).not.toContain("seedPhrase");
    expect(stored).not.toContain("galaxy stellar quantum");

    // The address survives - only the signing material is dropped.
    expect(stored).toContain("arc1qxywa87m9v3kz8n2p5nc4z8y7dv4q3lns8z3p");
  });

  test("the removed on-chain inference mode is coerced to coordinator", async ({
    page,
  }) => {
    await seedOnboardedLegacy(page);
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    // The on-chain radio was deleted but every default still said
    // "onchain", so this radio rendered unchecked on a fresh install.
    await expect(page.getByTestId("inference-mode-coordinator")).toBeChecked();
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

  test("community-mode only toggled for worker role WITH a model", () => {
    // The old regex was `if config.role == "worker" {` followed by
    // `--community-mode`, and it passed against the *log-line* formatting
    // rather than the spawn gate. The real gate has always additionally
    // required a model (a worker with no model would have the gateway
    // forward requests it cannot answer), so the test was green for the
    // wrong reason and would have stayed green if the gate were deleted.
    // Assert the actual argv condition.
    expect(src).toMatch(
      /if\s+config\.role\s*==\s*"worker"\s*&&\s*config\.model_path\.is_some\(\)\s*\{[\s\S]{0,200}?--community-mode/,
    );
  });

  test("never joins the public network as a staked validator", () => {
    // LIVE-NETWORK SAFETY (CLAUDE.md rules 2 and 3): arc-node's --stake
    // defaults to 5,000,000, and stake > 0 with a model set is what triggers
    // auto-shard-join. A desktop node must always announce stake 0.
    expect(src).toMatch(/\.arg\("--stake"\)\s*\n?\s*\.arg\("0"\)/);
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
    // The path is composed across two files now: paths.rs owns
    // `home_dir()/.arc` (so Windows resolves USERPROFILE instead of silently
    // falling back to `./.arc`), and node_manager.rs appends `bin/arc-node`.
    // The old single-file regex matched the literal `.arc"` that used to sit
    // inside managed_binary_path.
    const nm = fs.readFileSync(
      path.resolve(REPO_DESKTOP, "src-tauri", "src", "node_manager.rs"),
      "utf8",
    );
    const paths = fs.readFileSync(
      path.resolve(REPO_DESKTOP, "src-tauri", "src", "paths.rs"),
      "utf8",
    );
    expect(paths).toMatch(/fn arc_home\(\)[\s\S]*?home_dir\(\)\.join\("\.arc"\)/);
    expect(nm).toMatch(
      /fn managed_binary_path\(\)[\s\S]*?arc_home\(\)[\s\S]*?"bin"[\s\S]*?arc-node/,
    );
  });

  test("home dir resolution honours USERPROFILE, not just HOME", () => {
    // Windows normally has no HOME. Reading only HOME turned the default
    // `~/.arc` data dir into `./.arc` relative to the GUI's CWD - typically
    // an unwritable directory under Program Files.
    const paths = fs.readFileSync(
      path.resolve(REPO_DESKTOP, "src-tauri", "src", "paths.rs"),
      "utf8",
    );
    expect(paths).toMatch(/"HOME"/);
    expect(paths).toMatch(/"USERPROFILE"/);
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
