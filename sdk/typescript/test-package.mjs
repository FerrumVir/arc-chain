import { execFileSync } from "node:child_process";
import {
  cpSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const temporaryRoot = mkdtempSync(join(tmpdir(), "arc-sdk-package-"));
const packageRoot = fileURLToPath(new URL(".", import.meta.url));
const stagedPackage = join(temporaryRoot, "package-source");
const archivesRoot = join(temporaryRoot, "archives");
const consumerRoot = join(temporaryRoot, "consumer");
const npmExecPath = process.env.npm_execpath;

if (!npmExecPath) {
  throw new Error("npm_execpath is unavailable; run this smoke through npm");
}

function runNpm(args, options) {
  return execFileSync(process.execPath, [npmExecPath, ...args], options);
}

try {
  // Build and pack from an isolated copy. A quality check must never delete or
  // overwrite a developer's tracked dist/ changes through npm's prepack hook.
  mkdirSync(stagedPackage, { recursive: true });
  mkdirSync(archivesRoot, { recursive: true });
  mkdirSync(consumerRoot, { recursive: true });
  for (const entry of ["package.json", "README.md", "LICENSE", "src"]) {
    cpSync(join(packageRoot, entry), join(stagedPackage, entry), {
      recursive: true,
    });
  }
  const typescriptCli = join(
    packageRoot,
    "node_modules",
    "typescript",
    "lib",
    "tsc.js",
  );
  execFileSync(
    process.execPath,
    [
      typescriptCli,
      "-p",
      join(packageRoot, "tsconfig.json"),
      "--outDir",
      join(stagedPackage, "dist"),
    ],
    { cwd: packageRoot, stdio: "pipe" },
  );
  runNpm(["pack", "--ignore-scripts", "--pack-destination", archivesRoot], {
    cwd: stagedPackage,
    stdio: "pipe",
  });
  const archives = readdirSync(archivesRoot).filter((name) => name.endsWith(".tgz"));
  if (archives.length !== 1) {
    throw new Error(`expected one packed SDK archive, found ${archives.length}`);
  }

  writeFileSync(
    join(consumerRoot, "package.json"),
    JSON.stringify({ private: true, type: "module" }),
  );
  runNpm(
    [
      "install",
      "--ignore-scripts",
      "--no-audit",
      "--no-fund",
      "--no-package-lock",
      "--no-save",
      join(archivesRoot, archives[0]),
    ],
    { cwd: consumerRoot, stdio: "pipe" },
  );

  const smokeTest = join(consumerRoot, "smoke.mjs");
  writeFileSync(
    smokeTest,
    `import { ArcClient, formatArc, isValidAddress } from "@arc-chain/sdk";\n` +
      `if (typeof ArcClient !== "function") throw new Error("ArcClient export missing");\n` +
      `if (formatArc(2500000) !== "2.500000") throw new Error("formatArc export broken");\n` +
      `if (formatArc(9007199254740993n) !== "9007199254.740993") throw new Error("formatArc bigint precision broken");\n` +
      `if (!isValidAddress("a".repeat(64))) throw new Error("address validator broken");\n` +
      `const calls = [];\n` +
      `globalThis.fetch = async (url, init = {}) => {\n` +
      `  calls.push({ url: String(url), init });\n` +
      `  if (String(url).endsWith("/network/info")) return { ok: true, status: 200, text: async () => JSON.stringify({ protocol_version: "3.0.0", recovery_active: true, transaction_domain: "0x" + "11".repeat(32) }) };\n` +
      `  if (String(url).includes("/account/")) return { ok: true, status: 200, text: async () => '{"address":"' + "22".repeat(32) + '","balance":9007199254740993,"nonce":9007199254740995,"code_hash":"' + "00".repeat(32) + '","storage_root":"' + "00".repeat(32) + '","staked_balance":9007199254740997}' };\n` +
      `  return { ok: true, status: 200, text: async () => JSON.stringify({ tx_hash: "aa".repeat(32), status: "pending" }) };\n` +
      `};\n` +
      `const client = new ArcClient("http://127.0.0.1:9090");\n` +
      `await client.submitTx({ from: "22".repeat(32), to: "33".repeat(32), amount: 7, nonce: 4, fee: 1, signature: "44".repeat(64), public_key: "55".repeat(32), transaction_domain: "0x" + "11".repeat(32) });\n` +
      `const wire = JSON.parse(calls[1].init.body);\n` +
      `if (wire.transaction_domain !== undefined) throw new Error("local signing domain leaked into tx wire body");\n` +
      `if (wire.signature !== "44".repeat(64) || wire.public_key !== "55".repeat(32) || wire.fee !== 1) throw new Error("signed transfer wire contract broken");\n` +
      `await client.submitSignedTx({ tx_hash: "66".repeat(32), tx_type: "Transfer", from: "22".repeat(32), nonce: 5, fee: 1, gas_limit: 0, body: { type: "Transfer", to: "33".repeat(32), amount: 8, amount_commitment: null }, signature: { Ed25519: { signature: "77".repeat(64), public_key: "88".repeat(32) } }, transaction_domain: "0x" + "11".repeat(32) });\n` +
      `const normalized = JSON.parse(calls[3].init.body);\n` +
      `if (normalized.body !== undefined || normalized.tx_hash !== undefined || normalized.transaction_domain !== undefined) throw new Error("read projection leaked into signed transfer wire body");\n` +
      `if (normalized.to !== "33".repeat(32) || normalized.amount !== 8 || normalized.signature !== "77".repeat(64) || normalized.public_key !== "88".repeat(32)) throw new Error("submitSignedTx did not flatten the exact transfer wire contract");\n` +
      `const callsBeforeUnsafe = calls.length;\n` +
      `await client.submitTx({ from: "22".repeat(32), to: "33".repeat(32), amount: 9007199254740992, nonce: 4, fee: 1, signature: "44".repeat(64), public_key: "55".repeat(32), transaction_domain: "0x" + "11".repeat(32) }).then(() => { throw new Error("unsafe number was accepted"); }, (error) => { if (!(error instanceof RangeError) || !String(error.message).includes("safe integer")) throw error; });\n` +
      `if (calls.length !== callsBeforeUnsafe) throw new Error("unsafe u64 reached the network");\n` +
      `const batchEntry = { from: "22".repeat(32), to: "33".repeat(32), amount: 1, nonce: 0, fee: 1, signature: "44".repeat(64), public_key: "55".repeat(32), transaction_domain: "0x" + "11".repeat(32) };\n` +
      `await client.submitTxBatch(Array.from({ length: 65 }, () => batchEntry)).then(() => { throw new Error("oversized batch was accepted"); }, (error) => { if (!String(error.message).includes("maximum of 64 items")) throw error; });\n` +
      `if (calls.length !== callsBeforeUnsafe) throw new Error("oversized batch reached the network");\n` +
      `await client.submitTx({ from: "22".repeat(32), to: "33".repeat(32), amount: 9007199254740993n, nonce: 9007199254740995n, fee: 9007199254740997n, signature: "44".repeat(64), public_key: "55".repeat(32), transaction_domain: "0x" + "11".repeat(32) });\n` +
      `const exactWire = calls.at(-1).init.body;\n` +
      `if (!exactWire.includes('"amount":9007199254740993') || !exactWire.includes('"nonce":9007199254740995') || !exactWire.includes('"fee":9007199254740997')) throw new Error("bigint u64 wire values were not serialized exactly");\n` +
      `const account = await client.getAccount("22".repeat(32));\n` +
      `if (account.balance !== 9007199254740993n || account.nonce !== 9007199254740995n || account.staked_balance !== 9007199254740997n) throw new Error("u64 RPC response was rounded");\n`,
  );
  execFileSync(process.execPath, [smokeTest], {
    cwd: consumerRoot,
    stdio: "pipe",
  });

  const typeSmoke = join(consumerRoot, "consumer.ts");
  writeFileSync(
    typeSmoke,
    `import { ArcClient } from "@arc-chain/sdk";\n` +
      `import type { BatchSettleBody, CommunityInferenceRewardBody, JoinValidatorBody, TransactionBody, TxSubmitPayload, U64 } from "@arc-chain/sdk";\n` +
      `const joined = { type: "JoinValidator", pubkey: "00", initial_stake: 1 } satisfies JoinValidatorBody;\n` +
      `const batch = { type: "BatchSettle", entries: 1, total_amount: 1 } satisfies BatchSettleBody;\n` +
      `const reward = { type: "CommunityInferenceReward", chain_domain: "00", job_id: "00", worker: "00", model_id: "00", input_hash: "00", output_hash: "00", max_tokens: 1, expires_at_height: 1, worker_attestation_hash: "00" } satisfies CommunityInferenceRewardBody;\n` +
      `const bodies: TransactionBody[] = [joined, batch, reward];\n` +
      `const client = new ArcClient("http://127.0.0.1:9090");\n` +
      `const signed = { from: "00", to: "00", amount: 1, nonce: 0, fee: 1, signature: "00", public_key: "00", transaction_domain: null } satisfies TxSubmitPayload;\n` +
      `const exactAmount: U64 = 9007199254740993n;\n` +
      `const exactSigned = { ...signed, amount: exactAmount } satisfies TxSubmitPayload;\n` +
      `void client.submitTx(signed);\n` +
      `void client.submitTx(exactSigned);\n` +
      `// @ts-expect-error unsigned transaction submission must fail at compile time\n` +
      `void client.submitTx({ from: "00", to: "00", amount: 1, nonce: 0, fee: 1, transaction_domain: null });\n` +
      `void bodies;\n`,
  );
  const typeSmokeConfig = join(consumerRoot, "tsconfig.json");
  writeFileSync(
    typeSmokeConfig,
    JSON.stringify({
      compilerOptions: {
        target: "ES2022",
        module: "NodeNext",
        moduleResolution: "NodeNext",
        strict: true,
        noEmit: true,
        skipLibCheck: false,
      },
      include: ["consumer.ts"],
    }),
  );
  execFileSync(process.execPath, [typescriptCli, "-p", typeSmokeConfig], {
    cwd: consumerRoot,
    stdio: "pipe",
  });

  console.log("packed @arc-chain/sdk ESM import and declaration smoke passed");
} finally {
  rmSync(temporaryRoot, { recursive: true, force: true });
}
