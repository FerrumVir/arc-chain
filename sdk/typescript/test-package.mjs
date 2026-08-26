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
      `if (!isValidAddress("a".repeat(64))) throw new Error("address validator broken");\n` +
      `new ArcClient("http://127.0.0.1:9090");\n`,
  );
  execFileSync(process.execPath, [smokeTest], {
    cwd: consumerRoot,
    stdio: "pipe",
  });

  const typeSmoke = join(consumerRoot, "consumer.ts");
  writeFileSync(
    typeSmoke,
    `import { ArcClient } from "@arc-chain/sdk";\n` +
      `import type { BatchSettleBody, CommunityInferenceRewardBody, JoinValidatorBody, TransactionBody } from "@arc-chain/sdk";\n` +
      `const joined = { type: "JoinValidator", pubkey: "00", initial_stake: 1 } satisfies JoinValidatorBody;\n` +
      `const batch = { type: "BatchSettle", entries: 1, total_amount: 1 } satisfies BatchSettleBody;\n` +
      `const reward = { type: "CommunityInferenceReward", chain_domain: "00", job_id: "00", worker: "00", model_id: "00", input_hash: "00", output_hash: "00", max_tokens: 1, expires_at_height: 1, worker_attestation_hash: "00" } satisfies CommunityInferenceRewardBody;\n` +
      `const bodies: TransactionBody[] = [joined, batch, reward];\n` +
      `new ArcClient("http://127.0.0.1:9090");\n` +
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
