// ARC LOCAL DEVELOPMENT explorer view.
//
// Scope: one disposable local node, read directly over its RPC. This file is
// intentionally standalone - it never imports the canonical network resolver,
// never reads arc-network.json, and never writes a configuration document that
// the canonical explorer could consume. The canonical publication rules
// (approved recovery checkpoint + maintenance interlock) live in
// shared/frontend/arc-network.js and are untouched by this page.
(() => {
  "use strict";

  const SHORT = (hex, head = 10, tail = 6) => {
    const s = String(hex ?? "").replace(/^0x/, "");
    return s.length > head + tail ? `${s.slice(0, head)}…${s.slice(-tail)}` : s || "—";
  };

  function rpcBase() {
    const fromQuery = new URLSearchParams(window.location.search).get("rpc");
    if (fromQuery) return fromQuery.replace(/\/+$/, "");
    const meta = document.querySelector('meta[name="arc-localdev-rpc"]');
    return String(meta?.content || "http://127.0.0.1:9990").replace(/\/+$/, "");
  }

  const BASE = rpcBase();
  // Other replicas to cross-check a native receipt against: ?replicas=a,b
  const REPLICAS = (new URLSearchParams(window.location.search).get("replicas") || "")
    .split(",")
    .map((s) => s.trim().replace(/\/+$/, ""))
    .filter(Boolean);
  const NATIVE = window.ArcLocalDevNative;
  // Tracked so a native-request lookup - triggered independently of the
  // periodic refresh, e.g. from the deep link on page load - can still judge
  // Pending expiry against this node's own most recently observed height.
  let lastKnownHeight = null;

  async function rpc(path) {
    const response = await fetch(`${BASE}${path}`, { headers: { accept: "application/json" } });
    if (!response.ok) throw new Error(`${path} -> HTTP ${response.status}`);
    return response.json();
  }

  const $ = (id) => document.getElementById(id);

  function setBlocksMessage(text) {
    $("localdev-blocks-body").innerHTML =
      `<tr><td colspan="4" class="empty-cell">${text}</td></tr>`;
  }

  async function loadStatus() {
    const [health, info, validators] = await Promise.all([
      rpc("/health"),
      rpc("/info").catch(() => null),
      rpc("/validators").catch(() => null),
    ]);
    const height = Number(health?.height ?? 0);
    $("localdev-height").textContent = String(height);
    $("localdev-height-note").textContent =
      height > 0 ? "Committed by this local chain" : "No block committed yet";
    $("localdev-validators").textContent =
      validators?.validators ? String(validators.validators.length) : "—";
    $("localdev-accounts").textContent =
      info?.account_count !== undefined ? String(info.account_count) : "—";
    $("localdev-mempool").textContent =
      info?.mempool_size !== undefined ? String(info.mempool_size) : "—";
    $("localdev-network-label").textContent = `LOCAL DEV / HEIGHT ${height}`;
    lastKnownHeight = height;
    return { height, validators: validators?.validators ?? [] };
  }

  async function loadBlocks(height) {
    if (!height) {
      setBlocksMessage("The local node has not committed a block yet.");
      $("localdev-blocks-status").textContent = "Empty";
      return [];
    }
    const from = Math.max(0, height - 14);
    const payload = await rpc(`/blocks?from=${from}&to=${height}&limit=15`);
    const blocks = (payload?.blocks ?? []).slice().sort((a, b) => b.height - a.height);
    if (blocks.length === 0) {
      setBlocksMessage("The node reported a height but returned no blocks.");
      $("localdev-blocks-status").textContent = "Empty";
      return [];
    }
    const body = $("localdev-blocks-body");
    body.textContent = "";
    for (const block of blocks) {
      const row = document.createElement("tr");
      row.className = "localdev-block-row";
      row.dataset.height = String(block.height);
      row.innerHTML =
        `<td class="localdev-block-height">${block.height}</td>` +
        `<td class="localdev-block-txcount">${block.tx_count ?? 0}</td>` +
        `<td><code>${SHORT(block.producer, 8, 4)}</code></td>` +
        `<td><code>${SHORT(block.hash)}</code></td>`;
      row.addEventListener("click", () => void showBlock(block));
      body.appendChild(row);
    }
    $("localdev-blocks-status").textContent = `${blocks.length} blocks`;
    return blocks;
  }

  /// Normalise a raw /block/{h} response (header-nested) to the flat shape the
  /// /blocks listing uses, so one renderer serves both.
  function flattenBlock(raw) {
    const header = raw?.header ?? {};
    return {
      height: header.height,
      hash: raw?.hash,
      parent_hash: header.parent_hash,
      tx_root: header.tx_root,
      tx_count: header.tx_count,
      timestamp: header.timestamp,
      producer: header.producer,
    };
  }

  async function openBlockByHeight(height) {
    const clean = String(height ?? "").trim();
    if (!/^\d+$/.test(clean)) return;
    try {
      const raw = await rpc(`/block/${clean}`);
      await showBlock(flattenBlock(raw));
    } catch (error) {
      $("localdev-block-detail").innerHTML =
        `<p class="empty-cell">Could not open block ${clean}: ${error.message}</p>`;
      $("localdev-tx-list").textContent = "";
    }
  }

  async function showBlock(block) {
    const detail = $("localdev-block-detail");
    const list = $("localdev-tx-list");
    detail.innerHTML =
      `<dl class="localdev-detail-grid">` +
      `<dt>Height</dt><dd id="localdev-detail-height">${block.height}</dd>` +
      `<dt>Block hash</dt><dd><code id="localdev-detail-hash">${String(block.hash ?? "")}</code></dd>` +
      `<dt>Parent</dt><dd><code>${SHORT(block.parent_hash)}</code></dd>` +
      `<dt>Transaction root</dt><dd><code>${SHORT(block.tx_root)}</code></dd>` +
      `<dt>Transactions</dt><dd id="localdev-detail-txcount">${block.tx_count ?? 0}</dd>` +
      `</dl>`;
    list.textContent = "";
    let txs = [];
    try {
      const payload = await rpc(`/block/${block.height}/txs?limit=25`);
      txs = payload?.transactions ?? [];
    } catch (error) {
      const item = document.createElement("li");
      item.className = "empty-cell";
      item.textContent = `Could not load transactions: ${error.message}`;
      list.appendChild(item);
      return;
    }
    if (txs.length === 0) {
      const item = document.createElement("li");
      item.className = "empty-cell localdev-no-txs";
      item.textContent = "This block carries no transactions.";
      list.appendChild(item);
      return;
    }
    for (const tx of txs) {
      const item = document.createElement("li");
      item.className = "localdev-tx";
      item.dataset.hash = tx.hash;
      item.innerHTML = `<span class="localdev-tx-index">#${tx.index}</span> <code>${SHORT(tx.hash, 16, 8)}</code> <span class="localdev-tx-type"></span>`;
      list.appendChild(item);
      // The type and outcome come from the transaction itself; a native one
      // links to its request's receipt.
      rpc(`/tx/${tx.hash}/full`)
        .then((full) => {
          const d = NATIVE.describeTransaction(full);
          const label = item.querySelector(".localdev-tx-type");
          label.textContent = `${d.type}${d.success ? "" : " (failed)"}`;
          if (d.native && d.requestId) {
            const open = document.createElement("button");
            open.type = "button";
            open.className = "localdev-native-open";
            open.textContent = "Open request";
            open.addEventListener("click", () => void lookupNative(d.requestId));
            item.appendChild(open);
          }
        })
        .catch(() => {});
    }
  }

  async function receiptFrom(base, id) {
    try {
      const response = await fetch(`${base}/native-inference/receipt/${id}`, {
        headers: { accept: "application/json" },
      });
      return response.ok ? await response.json() : null;
    } catch {
      return null;
    }
  }

  async function lookupNative(requestId) {
    const clean = String(requestId || "").trim().replace(/^0x/, "");
    const note = $("localdev-native-note");
    $("localdev-native-input").value = clean;
    if (!/^[0-9a-fA-F]{64}$/.test(clean)) {
      note.textContent = "Enter a 64-character hex request id.";
      return;
    }
    const sources = [BASE, ...REPLICAS];
    const answers = await Promise.all(
      sources.map(async (source) => ({ source, receipt: await receiptFrom(source, clean) })),
    );
    const comparison = NATIVE.compareReplicas(answers);
    const own = comparison.rows[0].summary;
    const credits = $("localdev-native-credits");
    credits.textContent = "";
    const expiryNote = $("localdev-native-expiry");
    if (!own.known) {
      $("localdev-native-status").textContent = "No receipt on this node";
      note.textContent = "This node records no such request (not admitted, or not yet applied here).";
      expiryNote.hidden = true;
      return;
    }
    $("localdev-native-status").textContent = own.status;
    $("localdev-native-status").dataset.status = own.status;
    $("localdev-native-amounts").textContent = `${own.price} / ${own.reserved}`;
    $("localdev-native-credited").textContent = own.terminal
      ? `${own.credited} - ${own.reconciled ? "reconciles with the reservation" : "DOES NOT reconcile with the reservation"}`
      : "not settled";
    $("localdev-native-credited").dataset.reconciled = String(own.reconciled);
    $("localdev-native-admitted").textContent = own.admissionHeight ?? "-";
    $("localdev-native-settled").textContent = own.terminalHeight ?? "-";
    $("localdev-native-expires").textContent = own.expiresAt ?? "unavailable from this node";
    $("localdev-native-requester").textContent = own.requester ? SHORT(own.requester, 12, 8) : "-";
    $("localdev-native-output").textContent = own.outputHash ? SHORT(own.outputHash, 16, 8) : "-";
    $("localdev-native-output-hex").textContent = own.outputCertified ? SHORT(own.outputHex, 16, 8) : "empty - not finalized yet";
    $("localdev-native-output-text").textContent = own.outputText !== null ? own.outputText : "unavailable (display-only, never certified)";
    $("localdev-native-votes").textContent = own.votes ?? "-";
    // Read this node's OWN reported height, not another replica's, so the
    // expiry judgment matches whichever chain view is on screen.
    const expiry = NATIVE.classifyExpiry(own, lastKnownHeight);
    if (expiry.applicable && expiry.expired === true) {
      expiryNote.hidden = false;
      expiryNote.className = "truth-warn";
      expiryNote.textContent = "Expired without a certificate: refundable by a refund transaction. Not refunded yet - that happens only once such a transaction is mined.";
    } else if (expiry.applicable && expiry.expired === null) {
      expiryNote.hidden = false;
      expiryNote.className = "";
      expiryNote.textContent = "This node's current height is unavailable, so expiry cannot be evaluated.";
    } else {
      expiryNote.hidden = true;
      expiryNote.textContent = "";
    }
    $("localdev-native-replicas").textContent = REPLICAS.length
      ? `${comparison.answered} of ${comparison.asked} answered - ${comparison.agree ? "all agree" : "DISAGREE"}`
      : "not cross-checked (add ?replicas=)";
    $("localdev-native-replicas").dataset.agree = String(comparison.agree);
    for (const c of own.credits) {
      const item = document.createElement("li");
      item.className = "localdev-native-credit";
      item.innerHTML = `<code>${SHORT(c.payee, 12, 8)}</code> <span>${c.amount}</span>`;
      credits.appendChild(item);
    }
    note.textContent = "Read from the canonical receipt on each node; nothing here is inferred from a transaction hash.";
  }

  async function lookupBalance(address) {
    const clean = String(address || "").trim().replace(/^0x/, "");
    $("localdev-balance-note").textContent = "";
    if (!/^[0-9a-fA-F]{64}$/.test(clean)) {
      $("localdev-balance-note").textContent = "Enter a 64-character hex address.";
      return;
    }
    try {
      const account = await rpc(`/account/${clean}`);
      $("localdev-balance-address").textContent = SHORT(clean, 12, 8);
      $("localdev-balance-value").textContent = String(account.balance ?? 0);
      $("localdev-balance-nonce").textContent = String(account.nonce ?? 0);
    } catch (error) {
      $("localdev-balance-address").textContent = SHORT(clean, 12, 8);
      $("localdev-balance-value").textContent = "—";
      $("localdev-balance-nonce").textContent = "—";
      $("localdev-balance-note").textContent = `Lookup failed: ${error.message}`;
    }
  }

  // A pinned ?height= keeps the periodic refresh from pulling the detail pane
  // back to the newest block while someone is looking at an older one.
  const PINNED_HEIGHT = new URLSearchParams(window.location.search).get("height");

  async function refresh() {
    $("localdev-rpc-label").textContent = BASE;
    try {
      const { height, validators } = await loadStatus();
      const blocks = await loadBlocks(height);
      if (PINNED_HEIGHT) {
        await openBlockByHeight(PINNED_HEIGHT);
      } else if (blocks.length > 0) {
        // Show the newest block that actually carries transactions when there
        // is one; otherwise the newest block. Either way it is a real block
        // this node committed.
        await showBlock(blocks.find((b) => (b.tx_count ?? 0) > 0) ?? blocks[0]);
      }
      const input = $("localdev-account-input");
      if (validators.length > 0 && !input.value) {
        input.value = String(validators[0].address ?? "").replace(/^0x/, "");
        await lookupBalance(input.value);
      }
    } catch (error) {
      $("localdev-network-label").textContent = "LOCAL DEV / UNREACHABLE";
      setBlocksMessage(`Local node unreachable at ${BASE}: ${error.message}`);
      $("localdev-blocks-status").textContent = "Offline";
    }
  }

  document.addEventListener("DOMContentLoaded", () => {
    $("localdev-refresh").addEventListener("click", () => void refresh());
    $("localdev-balance-button").addEventListener("click", () =>
      void lookupBalance($("localdev-account-input").value),
    );
    $("localdev-block-button").addEventListener("click", () =>
      void openBlockByHeight($("localdev-block-input").value),
    );
    $("localdev-native-button").addEventListener("click", () =>
      void lookupNative($("localdev-native-input").value),
    );
    const deepLinked = new URLSearchParams(window.location.search).get("request");
    if (deepLinked) void lookupNative(deepLinked);
    if (PINNED_HEIGHT) $("localdev-block-input").value = PINNED_HEIGHT;
    void refresh();
    window.setInterval(() => void refresh(), 5000);
  });
})();
