#!/usr/bin/env node

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { CHECKS, REWARD, REWARD_BLOCK_ROOTS, REQUESTS, SOURCES, inspectorFacts, runCheck, validateObservation } from "./check.mjs";

let count = 0;
async function test(name, fn) {
  await fn();
  count += 1;
  process.stdout.write(`ok ${count} - ${name}\n`);
}

function observation(check) {
  const result = { url: check.url, cards: [], transactionLinks: [] };
  if (check.kind === "home") return {
    ...result,
    height: "3,900,001",
    storedHeight: "3,900,000",
    validators: "6",
    sources: ["nyc", "lax", "ams", "lhr", "nrt", "sgp"].map((region) => ({ id: `v3-${region}` })),
    bannerTitle: "Canonical recovery verified",
    bannerDetail: "ARC v3 - NYC · exact checkpoint and 6 v3 replica identities verified · liveness live",
  };
  if (check.kind === "tx") return {
    ...result,
    kicker: "Transaction / receipt",
    cards: [{
      fields: { Block: "3,794,543", Reward: "Earned · successful mined receipt" },
      receipts: [{
        label: "Community reward receipt", tx_hash: REWARD.hash, tx_type: "0x25",
        status: "mined_success", block_height: REWARD.block, worker: REWARD.worker,
        reward_arc: 2.5, reward_base: 2_500_000_000,
      }],
    }],
  };
  if (check.kind === "block-sources") return {
    url: check.url,
    perSource: SOURCES.map((sourceId) => ({
      sourceId,
      title: "Block #3,794,543",
      source: `ARC v3 - ${sourceId.slice(3).toUpperCase()} · ${sourceId.slice(3)}`,
      blockHash: REWARD_BLOCK_ROOTS.hash,
      stateRoot: REWARD_BLOCK_ROOTS.stateRoot,
    })),
  };
  if (check.kind === "block") return {
    ...result,
    title: "Block #3,794,543",
    transactionLinks: [{ hash: REWARD.hash, text: "0xbcccf7ba99…f785720" }],
  };
  return {
    ...result,
    kicker: "Native request · per-source receipts",
    cards: [{
      fields: { "Chain status": check.request.status, "Admitted at": "3,827,760" },
      receipts: [{
        label: "Receipt", request_id: check.request.id, observed_status: check.request.status,
        admission_transaction: { tx_hash: check.request.admissionHash, block_height: check.request.admissionBlock },
      }],
    }],
  };
}

await test("accepts rendered home, reward, block, native statuses and reload evidence", () => {
  for (const check of CHECKS) validateObservation(check, observation(check));
});

await test("rejects placeholders, stale heights, missing validators and incomplete agreement", () => {
  const check = CHECKS[0];
  for (const changes of [
    { height: "—" }, { storedHeight: "Loading…" }, { storedHeight: "138,311" },
    { validators: "5" }, { sources: observation(check).sources.slice(1) },
    { sources: Array(6).fill({ id: "v3-nyc" }) },
    { bannerTitle: "Canonical evidence incomplete" }, { bannerDetail: "5 v3 replica identities verified" },
  ]) assert.throws(() => validateObservation(check, { ...observation(check), ...changes }));
});

await test("rejects the right words on the wrong page", () => {
  for (const check of CHECKS) {
    assert.throws(() => validateObservation(check, { ...observation(check), url: "https://example.test/" }));
  }
});

await test("requires a successful 0x25 receipt with exact hash, block, worker and 2.5 ARC", () => {
  const check = CHECKS[1];
  for (const changes of [
    { tx_hash: REQUESTS[0].id }, { tx_type: "0x26" }, { status: "pending" },
    { block_height: REWARD.block + 1 }, { worker: REQUESTS[0].id },
    { reward_arc: 25 }, { reward_base: 250_000_000 },
  ]) {
    const observed = observation(check);
    Object.assign(observed.cards[0].receipts[0], changes);
    assert.throws(() => validateObservation(check, observed));
  }
});

await test("raw receipt numbers cannot replace the rendered earned reward and block", () => {
  const check = CHECKS[1];
  for (const fields of [
    { Block: "3,794,543", Reward: "Not counted as earned" },
    { Block: "Unavailable", Reward: "Earned · successful mined receipt" },
  ]) {
    const observed = observation(check);
    observed.cards[0].fields = fields;
    assert.throws(() => validateObservation(check, observed));
  }
});

await test("labels rendered upper-case by CSS are matched case-insensitively", () => {
  const reward = observation(CHECKS[1]);
  reward.cards[0].fields = { BLOCK: "3,794,543", REWARD: "Earned · successful mined receipt" };
  validateObservation(CHECKS[1], reward);
  reward.cards[0].fields = { BLOCK: "3,794,544", REWARD: "Earned · successful mined receipt" };
  assert.throws(() => validateObservation(CHECKS[1], reward));
  for (const check of CHECKS.filter((entry) => entry.kind === "request")) {
    const observed = observation(check);
    const fields = observed.cards[0].fields;
    observed.cards[0].fields = Object.fromEntries(Object.entries(fields).map(([key, value]) => [key.toUpperCase(), value]));
    validateObservation(check, observed);
    observed.cards[0].fields["CHAIN STATUS"] = "Pending";
    assert.throws(() => validateObservation(check, observed));
  }
});

await test("an opened block must render identical roots under every one of the six sources", () => {
  const check = CHECKS.find((entry) => entry.kind === "block-sources");
  assert.deepEqual([...check.sources].sort(), ["v3-ams", "v3-lax", "v3-lhr", "v3-nrt", "v3-nyc", "v3-sgp"]);
  for (const mutate of [
    (observed) => { observed.perSource.pop(); },
    (observed) => { observed.perSource[5] = { ...observed.perSource[0] }; },
    (observed) => { observed.perSource[2].stateRoot = "0x" + "0".repeat(64); },
    (observed) => { observed.perSource[4].blockHash = "0x" + "f".repeat(64); },
    (observed) => { observed.perSource[1].source = "ARC v3 - NYC · nyc"; },
    (observed) => { observed.perSource[3].title = "Block #3,794,544"; },
    (observed) => { observed.perSource[0].stateRoot = ""; },
  ]) {
    const observed = observation(check);
    mutate(observed);
    assert.throws(() => validateObservation(check, observed));
  }
  const facts = inspectorFacts({
    title: "Block #3,794,543",
    inspectorExcerpt: "CANONICAL STATUS\nCanonical\nSOURCE\nARC v3 - AMS · ams\nBLOCK HASH\n" + REWARD_BLOCK_ROOTS.hash +
      "\nPARENT HASH\n0x3cae\nSTATE ROOT\n" + REWARD_BLOCK_ROOTS.stateRoot + "\nTRANSACTIONS\n1",
  });
  assert.deepEqual(facts, { title: "Block #3,794,543", source: "ARC v3 - AMS · ams", blockHash: REWARD_BLOCK_ROOTS.hash, stateRoot: REWARD_BLOCK_ROOTS.stateRoot });
});

await test("block evidence needs the actual transaction list entry", () => {
  const check = CHECKS[2];
  for (const transactionLinks of [[], [{ hash: REQUESTS[0].id, text: REWARD.hash }], [{ hash: REWARD.hash, text: "" }]]) {
    assert.throws(() => validateObservation(check, { ...observation(check), transactionLinks, inspectorExcerpt: REWARD.hash }));
  }
  assert.throws(() => validateObservation(check, { ...observation(check), title: "Block #3,794,544" }));
});

await test("native requests require the correct identity and rendered terminal status", () => {
  for (const check of CHECKS.filter((entry) => entry.kind === "request")) {
    for (const mutate of [
      (card) => { card.fields["Chain status"] = "Pending"; },
      (card) => { card.receipts[0].observed_status = "Pending"; },
      (card) => { card.receipts[0].request_id = REWARD.hash; },
    ]) {
      const observed = observation(check);
      mutate(observed.cards[0]);
      assert.throws(() => validateObservation(check, observed));
    }
  }
});

await test("finalized receipt must include the expected admission transaction and block", () => {
  const check = CHECKS[3];
  for (const changes of [{ tx_hash: REWARD.hash }, { block_height: REWARD.block }]) {
    const observed = observation(check);
    Object.assign(observed.cards[0].receipts[0].admission_transaction, changes);
    assert.throws(() => validateObservation(check, observed));
  }
});

function fakePage(observed, screenshotError = null) {
  const calls = [];
  return {
    calls,
    url: () => observed.url,
    goto: async (url) => { calls.push(["goto", url]); },
    reload: async () => { calls.push(["reload"]); },
    evaluate: async () => observed,
    screenshot: async (options) => {
      calls.push(["screenshot", options]);
      if (screenshotError) throw new Error(screenshotError);
    },
  };
}

await test("failed assertions retain DOM and screenshot evidence and allow later checks", async () => {
  const check = CHECKS[0];
  const observed = { ...observation(check), height: "—" };
  const page = fakePage(observed);
  const failed = await runCheck(page, check, "unused", 0);
  assert.equal(failed.pass, false);
  assert.match(failed.error, /live reported height/);
  assert.deepEqual(failed.observed, observed);
  assert.equal(failed.screenshot, "home.png");
  assert.equal(page.calls[1][0], "screenshot");
  const next = CHECKS[1];
  assert.equal((await runCheck(fakePage(observation(next)), next, "unused", 0)).pass, true);
});

await test("navigation and screenshot failures cannot produce passing checks", async () => {
  const check = CHECKS[0];
  const page = fakePage(observation(check));
  page.goto = async () => { throw new Error("navigation timed out"); };
  const failed = await runCheck(page, check, "unused", 0);
  assert.equal(failed.pass, false);
  assert.equal(failed.error, "navigation timed out");
  assert.equal(failed.screenshot, "home.png");
  const noScreenshot = await runCheck(fakePage(observation(check), "screenshot failed"), check, "unused", 0);
  assert.equal(noScreenshot.pass, false);
  assert.equal(noScreenshot.screenshotError, "screenshot failed");
});

await test("reload performs a real reload and revalidates the deep link", async () => {
  const check = CHECKS.find((entry) => entry.reload);
  const page = fakePage(observation(check));
  assert.equal((await runCheck(page, check, "unused", 0)).pass, true);
  assert.deepEqual(page.calls[0], ["reload"]);
  const wrongRoute = fakePage({ ...observation(check), url: CHECKS[0].url });
  assert.equal((await runCheck(wrongRoute, check, "unused", 0)).pass, false);
  assert.equal(wrongRoute.calls.some(([method]) => method === "reload"), false);
});

await test("the six-source check selects every source and fails on one divergent root", async () => {
  const check = CHECKS.find((entry) => entry.kind === "block-sources");
  const rendered = (sourceId, stateRoot = REWARD_BLOCK_ROOTS.stateRoot) => ({
    url: check.url,
    title: "Block #3,794,543",
    inspectorExcerpt: `SOURCE\nARC v3 - ${sourceId.slice(3).toUpperCase()} · ${sourceId.slice(3)}\nBLOCK HASH\n${REWARD_BLOCK_ROOTS.hash}\nSTATE ROOT\n${stateRoot}`,
  });
  const sourcePage = (divergent = null) => {
    let selected = "canonical";
    const page = fakePage({ url: check.url });
    page.selectOption = async (selector, value) => { page.calls.push(["select", selector, value]); selected = value; };
    page.evaluate = async () => rendered(selected, selected === divergent ? "0x" + "1".repeat(64) : REWARD_BLOCK_ROOTS.stateRoot);
    return page;
  };
  const page = sourcePage();
  const passed = await runCheck(page, check, "unused", 0);
  assert.equal(passed.pass, true, passed.error);
  assert.deepEqual(page.calls.filter(([method]) => method === "select").map((call) => call[2]), SOURCES);
  const failed = await runCheck(sourcePage("v3-nrt"), check, "unused", 0);
  assert.equal(failed.pass, false);
  assert.match(failed.error, /v3-nrt state root/);
  assert.equal(failed.observed.perSource.length, 6);
});

await test("workflow stays branch-only, read-only, pinned and always uploads evidence", () => {
  const workflow = readFileSync(new URL("../../.github/workflows/ci-explorer-render.yml", import.meta.url), "utf8");
  assert.match(workflow, /on:\n  push:\n    branches: \['ci\/explorer-render-\*'\]/);
  assert.doesNotMatch(workflow, /pull_request|workflow_dispatch|schedule:|write/);
  assert.match(workflow, /permissions:\n  contents: read/);
  assert.match(workflow, /runs-on: ubuntu-24\.04/);
  const actions = [...workflow.matchAll(/uses: (\S+)/g)].map((match) => match[1]);
  assert.equal(actions.length, 3);
  for (const action of actions) assert.match(action, /^actions\/[a-z-]+@[a-f0-9]{40}$/);
  assert.match(workflow, /if: always\(\)\n\s+uses: actions\/upload-artifact@/);
  assert.match(workflow, /npx -y playwright@1\.59\.1 install --with-deps chromium/);
  assert.match(workflow, /--ignore-scripts playwright@1\.59\.1/);
});
