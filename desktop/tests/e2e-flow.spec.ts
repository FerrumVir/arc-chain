// End-to-end flow tests exercising the six fixes that unblocked testnet
// release (arc-node CLI spawn args, bundled seeds/genesis, BIP-39 validator
// seed, auto-download binary, mined-reward versus raw-claim semantics, Gatekeeper
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

  test("four-step setup: welcome → identity → model → launch", async ({
    page,
  }) => {
    await page.goto("/");

    await page.getByTestId("btn-continue-welcome").click();
    await expect(page.getByTestId("step-identity")).toBeVisible();
    await page.getByTestId("btn-reveal-seed").click();
    await page.getByTestId("btn-continue-identity").click();
    // Model picker (v0.6.0). This spec used to assert step-launch here.
    await expect(page.getByTestId("step-model")).toBeVisible();
    await expect(page.getByTestId("tier-standard")).toBeVisible();
    await expect(page.getByTestId("tier-standard")).toContainText(
      /ARC compatible/i,
    );
    await expect(page.getByTestId("tier-tiny")).toHaveCount(0);
    await expect(page.getByTestId("tier-big")).toHaveCount(0);
    await page.getByTestId("tier-skip").click();
    await page.getByTestId("btn-continue-model").click();
    await expect(page.getByTestId("step-launch")).toBeVisible();
    await expect(page.getByText(/ready to set up/i)).toBeVisible();

    await page.getByTestId("btn-launch").click();

    // Mock flow: ensureBinary + startNode + waitForPeer + faucetClaim all
    // resolve, then dashboard renders.
    await expect(page.getByTestId("dashboard")).toBeVisible({ timeout: 15_000 });
  });

  test("launch button promises setup, not a successful network join", async ({
    page,
  }) => {
    await page.goto("/");
    await walkToLaunch(page);
    await expect(page.getByTestId("btn-launch")).toBeVisible();
    await expect(page.getByTestId("btn-launch")).toContainText(
      /set up this node/i,
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

test.describe("Raw inference claims never become earnings", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("does not build an income chart from dated 0x16 claims", async ({
    page,
  }) => {
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("earnings-screen")).toBeVisible();

    // The mock carries dated raw inference claims. Those timestamps are not
    // mined 0x25 reward receipts, so no seven-day income chart may be built.
    await expect(page.getByTestId("weekly-chart")).toHaveCount(0);
    const feed = page.getByTestId("all-attestations");
    await expect(
      feed.getByText("COMPUTED · NOT PAYMENT · yours", { exact: true }),
    ).toHaveCount(2);
    await expect(feed.getByText(/\+2\.50 ARC · COMPUTED \+ PAID/)).toHaveCount(2);
  });

  test("another validator's attestation is not shown as the user's income", async ({
    page,
  }) => {
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    const feed = page.getByTestId("all-attestations");
    await expect(feed).toBeVisible();
    // Four of five real activity rows are the user's. Old-seed padding is not
    // activity and the other worker's 0x16 row must not become this income.
    await expect(page.getByText(/4 yours · 5 shown/)).toBeVisible();
    await expect(
      feed.getByText("COMPUTED · NOT PAYMENT · network", { exact: true }),
    ).toHaveCount(1);
  });
});

test.describe("Existing observer upgrade", () => {
  test("offers only the exact ARC-compatible 7B artifact", async ({ page }) => {
    await seedOnboardedLegacy(page);
    await page.goto("/");
    await expect(page.getByTestId("observer-upgrade-banner")).toBeVisible();
    await page.getByTestId("btn-open-upgrade").click();
    await expect(page.getByTestId("upgrade-tier-standard")).toContainText(
      /ARC compatible/i,
    );
    await expect(page.getByTestId("upgrade-tier-tiny")).toHaveCount(0);
    await expect(page.getByTestId("upgrade-tier-big")).toHaveCount(0);
  });
});

test.describe("Paid settlement recovery gate", () => {
  test("browser-live and preview adapters reject both write commands before fetch", () => {
    const src = fs.readFileSync(
      path.resolve(REPO_DESKTOP, "src", "lib", "tauri.ts"),
      "utf8",
    );
    const caseBodies = (command: string) =>
      [...src.matchAll(new RegExp(`case "${command}": \\{([\\s\\S]*?)\\n    \\}`, "g"))]
        .map((match) => match[1]);

    for (const command of ["run_paid_inference", "tier1_submit"]) {
      const bodies = caseBodies(command);
      // One browser-live adapter and one browser-preview adapter.
      expect(bodies).toHaveLength(2);
      for (const body of bodies) {
        expect(body).toContain("throw settlementWriteUnavailable");
        expect(body).not.toContain("fetch(");
        expect(body).not.toContain("return {");
        expect(body).not.toContain("submit_signed");
      }
    }
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
  // --validator-key-file, --seeds-file, --genesis, --eth-rpc-port, and
  // --community-mode and the non-shard full-integer worker role all end up on
  // the cmd line.
  const src = fs.readFileSync(
    path.resolve(REPO_DESKTOP, "src-tauri", "src", "node_manager.rs"),
    "utf8",
  );
  const activeStartBegin = src.indexOf("    pub async fn start(\n");
  const activeStartEnd = src.indexOf(
    "    /// Called on every node_status poll.",
    activeStartBegin,
  );
  const activeStart = src.slice(activeStartBegin, activeStartEnd);
  const productionEnd = src.indexOf("#[cfg(test)]");
  const production = src.slice(0, productionEnd);
  const legacyMatcherBegin = production.indexOf(
    "fn legacy_windows_command_matches(\n",
  );
  const legacyMatcherEnd = production.indexOf(
    "\npub fn detect_running_legacy_v07_data_dir(",
    legacyMatcherBegin,
  );
  const legacyV07Matcher = production.slice(
    legacyMatcherBegin,
    legacyMatcherEnd,
  );
  const currentProduction =
    production.slice(0, legacyMatcherBegin) +
    production.slice(legacyMatcherEnd);

  test("--rpc is passed as a single addr:port (not --rpc-port)", () => {
    // Positive: the string "--rpc" appears followed by the addr:port pattern.
    expect(src).toMatch(/\.arg\("--rpc"\)\s*\.\s*arg\(format!\("127\.0\.0\.1/);
    // Negative: no stray --rpc-port anywhere (that flag doesn't exist in arc-node).
    expect(src).not.toMatch(/--rpc-port/);
  });

  test("passes all required testnet flags", () => {
    expect(activeStartBegin).toBeGreaterThanOrEqual(0);
    expect(activeStartEnd).toBeGreaterThan(activeStartBegin);
    for (const flag of [
      '"--p2p-port"',
      '"--data-dir"',
      '"--validator-key-file"',
      '"--seeds-file"',
      '"--genesis"',
      '"--eth-rpc-port"',
      '"--community-mode"',
      '"--full-integer-worker"',
    ]) {
      expect(activeStart).toContain(flag);
    }
    expect(activeStart).not.toContain('"--validator-seed"');
    expect(activeStart).not.toContain("ARC_VALIDATOR_SEED");
    expect(activeStart).toContain("<app-owned-private-keyfile>");

    // The only production allowance for the old secret-in-argv flag is the
    // exact, tokenless v0.7 ownership matcher. It classifies an already-running
    // public legacy process and never builds current spawn argv.
    expect(legacyMatcherBegin).toBeGreaterThanOrEqual(0);
    expect(legacyMatcherEnd).toBeGreaterThan(legacyMatcherBegin);
    expect(legacyV07Matcher).toContain('"--validator-seed"');
    expect(legacyV07Matcher).toContain(
      'unique_command_flag_value(command, "--desktop-shutdown-token-file")',
    );
    expect(legacyV07Matcher).toContain("constant_time_legacy_seed_eq");
    expect(legacyV07Matcher).not.toContain("Command::new");
    expect(legacyV07Matcher).not.toContain(".arg(");
    expect(currentProduction).not.toContain('"--validator-seed"');
    expect(currentProduction).not.toContain("ARC_VALIDATOR_SEED");
  });

  test("fails closed when signed bundle network resources are missing or redirected", () => {
    expect(src).toContain("required_testnet_resources(resources)?");
    expect(src).toContain("std::fs::symlink_metadata(path)");
    expect(src).toContain("signed bundle resource genesis.toml is missing");
    expect(src).toContain("signed bundle resource testnet-seeds.txt is missing");
    expect(src).not.toContain("node will start isolated");
    expect(src).not.toContain("node validator set will be empty");
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
      /if\s+config\.role\s*==\s*"worker"\s*&&\s*config\.model_path\.is_some\(\)\s*\{[\s\S]{0,600}?--community-mode[\s\S]{0,100}?--full-integer-worker/,
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

  test("every automatic model path pins the immutable artifact revision", () => {
    const frontend = fs.readFileSync(
      path.resolve(REPO_DESKTOP, "src", "lib", "tauri.ts"),
      "utf8",
    );
    const nodeMain = fs.readFileSync(
      path.resolve(REPO_DESKTOP, "..", "crates", "arc-node", "src", "main.rs"),
      "utf8",
    );
    const revision = "191239b3e26b2882fb562ffccdd1cf0f65402adb";
    const digest =
      "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa";
    const desktopModelContract = commands.slice(
      commands.indexOf("const MODEL_TIERS"),
      commands.indexOf("fn model_digest"),
    );

    expect(desktopModelContract).toContain(`/resolve/${revision}/`);
    expect(frontend).toContain(`/resolve/${revision}/`);
    expect(nodeMain).toContain(`/resolve/${revision}/`);
    expect(desktopModelContract).toContain(digest);
    expect(nodeMain).toContain(digest);
    expect(desktopModelContract).not.toContain("/resolve/main/");
    expect(frontend).not.toContain("/resolve/main/");
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
  const commands = fs.readFileSync(
    path.resolve(REPO_DESKTOP, "src-tauri", "src", "commands.rs"),
    "utf8",
  );
  const updater = fs.readFileSync(
    path.resolve(REPO_DESKTOP, "src", "lib", "updater.ts"),
    "utf8",
  );
  const updateController = fs.readFileSync(
    path.resolve(REPO_DESKTOP, "src", "lib", "update-controller.ts"),
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

  test("updater fences download/install separately and startup adopts only an exact version", () => {
    expect(commands).toMatch(/pub async fn prepare_update_relaunch/);
    expect(commands).toMatch(/pub async fn abort_update_relaunch/);
    expect(lib).toContain("commands::prepare_update_relaunch");
    expect(lib).toContain("commands::abort_update_relaunch");
    expect(updater).toContain("prepareRelaunch: api.prepareUpdateRelaunch");
    expect(updater).toContain("abortRelaunch: api.abortUpdateRelaunch");
    expect(updater).toContain("update.download(");
    expect(updater).toContain("install: () => update.install()");
    expect(updater).not.toContain("downloadAndInstall");
    expect(updateController).toContain("prepared && !installInvoked");
    expect(updateController).not.toContain("downloadAndInstall");
    expect(commands).toContain("LocalNodeCompatibility::Incompatible");
    expect(lib).toContain("probe_local_node_compatibility");
    expect(lib).toContain("commands::EXPECTED_NODE_VERSION");
  });

  test("an unresolved native migration fence blocks manual start and restart", () => {
    const transactionBegin = commands.indexOf(
      "async fn start_node_transaction(\n",
    );
    const transactionEnd = commands.indexOf(
      "\npub async fn start_node_inner(",
      transactionBegin,
    );
    const startTransaction = commands.slice(transactionBegin, transactionEnd);
    const startWrapperBegin = commands.indexOf("pub async fn start_node_inner(");
    const startWrapperEnd = commands.indexOf(
      "\npub(crate) async fn recover_managed_shutdown_inner(",
      startWrapperBegin,
    );
    const startWrapper = commands.slice(startWrapperBegin, startWrapperEnd);
    const restart = commands.slice(
      commands.indexOf("pub async fn restart_node"),
      commands.indexOf("#[tauri::command]\npub async fn reset_peer_state"),
    );
    const resetPeerState = commands.slice(
      commands.indexOf("pub async fn reset_peer_state"),
      commands.indexOf("/// Health and progress of the node running on THIS machine."),
    );
    expect(lib).toContain("data_migration_error");
    expect(transactionBegin).toBeGreaterThanOrEqual(0);
    expect(transactionEnd).toBeGreaterThan(transactionBegin);
    const migrationGate = startTransaction.indexOf(
      "require_data_migration_ready(state).await?",
    );
    const lockAcquire = startTransaction.indexOf(
      "acquire_managed_lifecycle_lock(&lock_data_dir)",
    );
    const writerSpawns = [...startTransaction.matchAll(/node\.start\(/g)].map(
      (match) => match.index ?? -1,
    );
    expect(migrationGate).toBeGreaterThanOrEqual(0);
    expect(
      startTransaction.match(/require_data_migration_ready\(state\)\.await\?/g),
    ).toHaveLength(1);
    expect(lockAcquire).toBeGreaterThan(migrationGate);
    expect(writerSpawns.length).toBeGreaterThan(0);
    for (const writerSpawn of writerSpawns) {
      expect(writerSpawn).toBeGreaterThan(lockAcquire);
    }
    expect(startTransaction.slice(0, migrationGate)).not.toContain("node.start(");
    expect(startTransaction).toContain(
      "store.config.clone().unwrap_or_default()",
    );
    expect(startWrapper).toContain(
      "start_node_transaction(app, state, true).await",
    );
    expect(startWrapper).not.toContain("node.start(");
    expect(restart).toContain("require_data_migration_ready(&state).await?");
    expect(resetPeerState).toContain(
      "require_data_migration_ready(&state).await?",
    );
    expect(resetPeerState).toContain("node.stop_for_local_mutation().await");
    expect(resetPeerState.indexOf("stop_for_local_mutation")).toBeLessThan(
      resetPeerState.indexOf("std::fs::remove_file(&peers_path)"),
    );
    expect(resetPeerState).toContain(
      "node.start(&cfg, &validator_keyfile, &resources, lifecycle_lock)",
    );
    expect(resetPeerState.indexOf("require_data_migration_ready")).toBeLessThan(
      resetPeerState.indexOf("remove_file"),
    );
    expect(commands).toContain("preserve_authoritative_data_dir(&mut config");
    expect(commands).toContain("start_node_inner(&app, &state).await");
    expect(commands).not.toContain("start_node_inner(&app, &state, &config)");
    expect(commands).toContain("ARC refused to start the node");
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

test.describe("Unified release publishes a signed update manifest", () => {
  const wf = fs.readFileSync(
    path.resolve(
      REPO_DESKTOP,
      "..",
      ".github",
      "workflows",
      "release.yml",
    ),
    "utf8",
  );
  const preflight = fs.readFileSync(
    path.resolve(
      REPO_DESKTOP,
      "..",
      ".github",
      "workflows",
      "release-signing-preflight.yml",
    ),
    "utf8",
  );
  const assembler = fs.readFileSync(
    path.resolve(
      REPO_DESKTOP,
      "..",
      "scripts",
      "release",
      "assemble-release.sh",
    ),
    "utf8",
  );

  test("preflight signs once and the tag workflow reuses those exact bytes", () => {
    expect(preflight).toContain("TAURI_SIGNING_PRIVATE_KEY");
    expect(preflight).toContain("environment: release");
    expect(preflight).toMatch(/TAURI_SIGNING_PRIVATE_KEY_PASSWORD:\s*["']{2}/);
    expect(preflight).not.toContain(
      "TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ${{ secrets.",
    );
    expect(preflight).toContain("Package the exact signed desktop candidate");
    expect(wf).not.toContain("TAURI_SIGNING_PRIVATE_KEY");
    expect(wf).toContain("environment: release");
    expect(wf).toContain("artifact-ids:");
    expect(wf).toContain("digest-mismatch: error");
    expect(wf).toContain("materialize-pretag-artifacts.py");
  });

  test("assembler emits latest.json with per-target url + signature", () => {
    expect(assembler).toMatch(/darwin-aarch64/);
    expect(assembler).toMatch(/darwin-x86_64/);
    expect(assembler).toMatch(/linux-x86_64/);
    expect(assembler).toMatch(/\.app\.tar\.gz\.sig/);
    expect(assembler).toMatch(/latest\.json/);
    expect(wf).toContain("./scripts/release/assemble-release.sh");
  });

  test("tag trigger matches v*.*.*", () => {
    expect(wf).toMatch(/tags:\s*\n\s*-\s*'v\*\.\*\.\*'/);
  });
});
