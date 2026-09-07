import { defineConfig } from "@playwright/test";
import { createHash } from "node:crypto";
import { lstatSync, readFileSync, realpathSync } from "node:fs";
import { isAbsolute } from "node:path";
import baseConfig from "./playwright.config";

const required = [
  "ARC_LIVE_PORT",
  "ARC_LIVE_WORKER",
  "ARC_LIVE_REWARD_TX",
  "ARC_LIVE_PLAYWRIGHT_REPORT",
  "ARC_LIVE_APPIMAGE_RECEIPT",
  "ARC_LIVE_APP_ARCHIVE",
  "ARC_LIVE_APP_ARCHIVE_SIGNATURE",
  "ARC_LIVE_DMG",
  "ARC_LIVE_MACOS_CONTROLLER_ATTEMPT",
  "ARC_LIVE_MACOS_PACKAGE_INSPECTION",
  "ARC_LIVE_MACOS_PACKAGE_PROVENANCE",
  "ARC_LIVE_MACOS_PACKAGE_PROVENANCE_VERIFICATION",
  "ARC_LIVE_MACOS_UPDATER_SIGNATURE_RECEIPT",
  "ARC_LIVE_NATIVE_ATTEMPT",
  "ARC_LIVE_NATIVE_HOME",
  "ARC_LIVE_NATIVE_INPUT",
  "ARC_LIVE_NATIVE_RECEIPT",
  "ARC_LIVE_CONFIG",
  "ARC_LIVE_CONFIG_SHA256",
  "ARC_LIVE_SOURCE_COMMIT",
  "ARC_LIVE_RECEIPT_OUTPUT",
  "ARC_LIVE_NODE_ARCHIVE",
  "ARC_LIVE_NODE_ARCHIVE_SHA256",
  "ARC_LIVE_NODE_PATH",
  "ARC_LIVE_NODE_SHA256",
  "ARC_LIVE_NPM_CLI",
  "ARC_LIVE_NPM_CLI_SHA256",
  "ARC_LIVE_NPM_PACKAGE",
  "ARC_LIVE_NPM_PACKAGE_SHA256",
  "ARC_LIVE_ROLLOUT_MANIFEST_SHA256",
  "ARC_LIVE_SSH_PATH",
  "ARC_LIVE_SSH_SHA256",
  "ARC_LIVE_SSH_KNOWN_HOSTS",
  "ARC_LIVE_SSH_KNOWN_HOSTS_SHA256",
  "ARC_LIVE_SSH_IDENTITY_SHA256",
  "ARC_LIVE_VALIDATOR_HOST",
  "ARC_LIVE_VALIDATOR_NAME",
  "ARC_LIVE_VALIDATOR_RPC_SOCKET",
] as const;
for (const name of required) {
  if (!process.env[name]) throw new Error(`${name} is required for the fail-closed live product gate`);
}
if (!/^\d+$/.test(process.env.ARC_LIVE_PORT!)) {
  throw new Error("ARC_LIVE_PORT must be a decimal TCP port");
}
const livePort = Number(process.env.ARC_LIVE_PORT);
if (!Number.isSafeInteger(livePort) || livePort < 1 || livePort > 65_535) {
  throw new Error("ARC_LIVE_PORT must be between 1 and 65535");
}
for (const name of ["ARC_LIVE_WORKER", "ARC_LIVE_REWARD_TX"] as const) {
  if (!/^(?:0x)?[0-9a-f]{64}$/i.test(process.env[name]!)) {
    throw new Error(`${name} must be an exact 32-byte hexadecimal identity`);
  }
}
for (const name of [
  "ARC_LIVE_PLAYWRIGHT_REPORT",
  "ARC_LIVE_APPIMAGE_RECEIPT",
  "ARC_LIVE_APP_ARCHIVE",
  "ARC_LIVE_APP_ARCHIVE_SIGNATURE",
  "ARC_LIVE_DMG",
  "ARC_LIVE_MACOS_CONTROLLER_ATTEMPT",
  "ARC_LIVE_MACOS_PACKAGE_INSPECTION",
  "ARC_LIVE_MACOS_PACKAGE_PROVENANCE",
  "ARC_LIVE_MACOS_PACKAGE_PROVENANCE_VERIFICATION",
  "ARC_LIVE_MACOS_UPDATER_SIGNATURE_RECEIPT",
  "ARC_LIVE_NATIVE_ATTEMPT",
  "ARC_LIVE_NATIVE_HOME",
  "ARC_LIVE_NATIVE_INPUT",
  "ARC_LIVE_NATIVE_RECEIPT",
  "ARC_LIVE_CONFIG",
  "ARC_LIVE_RECEIPT_OUTPUT",
  "ARC_LIVE_NODE_ARCHIVE",
  "ARC_LIVE_NODE_PATH",
  "ARC_LIVE_NPM_CLI",
  "ARC_LIVE_NPM_PACKAGE",
  "ARC_LIVE_SSH_PATH",
  "ARC_LIVE_SSH_KNOWN_HOSTS",
] as const) {
  if (!isAbsolute(process.env[name]!)) {
    throw new Error(`${name} must be an absolute path`);
  }
}
const expectedHashes = {
  ARC_LIVE_NODE_ARCHIVE_SHA256: "b7bf7707070b950ba1ec5f1af3bb6de0f2b1962c5033973d94068ab021ef3014",
  ARC_LIVE_NODE_SHA256: "9d050fd455b56426e25d4d603c7c501cbb2630348e836cf221dcce748e90588a",
  ARC_LIVE_NPM_CLI_SHA256: "8e5f6f3429f8cdbe693cdc29904e9d5a7b127a494bd15c804bd54c7403bfcbe7",
  ARC_LIVE_NPM_PACKAGE_SHA256: "09dfcf187178ce1ab3ea6194c80d3ae082ad2a86dc1269ac963f94429e718122",
  ARC_LIVE_SSH_SHA256: "75ae4b414b57e0c52ad1cb24a9d7dae2496071fdf153c7fc8e94db3c9c4b0faa",
  ARC_LIVE_SSH_KNOWN_HOSTS_SHA256: "97c826f7e1a3940f6d18095ccdb0eaeebb5d66ec16fe60b9c5c47690e707485d",
  ARC_LIVE_SSH_IDENTITY_SHA256: "9a7b57700dc7acf0faeca152fc341f237704e81965b5a9656fe8ccee4931444a",
} as const;
for (const [name, expected] of Object.entries(expectedHashes)) {
  if (process.env[name] !== expected) {
    throw new Error(`${name} differs from the reviewed desktop-live tool/input identity`);
  }
}
for (const name of ["ARC_LIVE_CONFIG_SHA256", "ARC_LIVE_ROLLOUT_MANIFEST_SHA256"] as const) {
  if (!/^[0-9a-f]{64}$/.test(process.env[name]!)) {
    throw new Error(`${name} must be a lowercase SHA-256`);
  }
}
const fileHash = (name: string) => {
  const path = process.env[name]!;
  const info = lstatSync(path);
  if (!info.isFile() || info.isSymbolicLink()) {
    throw new Error(`${name} must select a non-symlink regular file`);
  }
  return createHash("sha256").update(readFileSync(path)).digest("hex");
};
for (const [pathName, hashName] of [
  ["ARC_LIVE_CONFIG", "ARC_LIVE_CONFIG_SHA256"],
  ["ARC_LIVE_NODE_ARCHIVE", "ARC_LIVE_NODE_ARCHIVE_SHA256"],
  ["ARC_LIVE_NODE_PATH", "ARC_LIVE_NODE_SHA256"],
  ["ARC_LIVE_NPM_CLI", "ARC_LIVE_NPM_CLI_SHA256"],
  ["ARC_LIVE_NPM_PACKAGE", "ARC_LIVE_NPM_PACKAGE_SHA256"],
  ["ARC_LIVE_SSH_PATH", "ARC_LIVE_SSH_SHA256"],
  ["ARC_LIVE_SSH_KNOWN_HOSTS", "ARC_LIVE_SSH_KNOWN_HOSTS_SHA256"],
] as const) {
  if (fileHash(pathName) !== process.env[hashName]) {
    throw new Error(`${pathName} differs from ${hashName}`);
  }
}
if (realpathSync(process.env.ARC_LIVE_NODE_PATH!) !== realpathSync(process.execPath)) {
  throw new Error("Playwright is not running under ARC_LIVE_NODE_PATH");
}
if (process.version !== "v24.20.0" || process.platform !== "darwin" || process.arch !== "arm64") {
  throw new Error("desktop live gate requires official Node v24.20.0 on native Apple Silicon macOS");
}
if (process.env.ARC_LIVE_VALIDATOR_NAME !== "lax" || process.env.ARC_LIVE_VALIDATOR_HOST !== "140.82.16.112") {
  throw new Error("desktop live gate must use the exact reviewed LAX validator");
}
const expectedSocket = `/run/arc-v3-rpc-lax-${process.env.ARC_LIVE_ROLLOUT_MANIFEST_SHA256!.slice(0, 16)}/rpc.sock`;
if (process.env.ARC_LIVE_VALIDATOR_RPC_SOCKET !== expectedSocket) {
  throw new Error("ARC_LIVE_VALIDATOR_RPC_SOCKET is not bound to the rollout manifest");
}
if (!/^[0-9a-f]{40}$/.test(process.env.ARC_LIVE_SOURCE_COMMIT!)) {
  throw new Error("ARC_LIVE_SOURCE_COMMIT must be a full lowercase Git commit");
}
process.env.ARC_LIVE_REQUIRED = "1";

export default defineConfig({
  ...baseConfig,
  forbidOnly: true,
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [
    ["list"],
    ["json", { outputFile: process.env.ARC_LIVE_PLAYWRIGHT_REPORT! }],
  ],
  testMatch: "**/live.spec.ts",
  webServer: baseConfig.webServer
    ? { ...baseConfig.webServer, reuseExistingServer: false }
    : undefined,
});
