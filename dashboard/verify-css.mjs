#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";

const outputUrl = new URL("./tailwind.css", import.meta.url);
const before = readFileSync(outputUrl);
const npm = process.platform === "win32" ? "npm.cmd" : "npm";

execFileSync(npm, ["run", "build:css"], { stdio: "inherit" });

const after = readFileSync(outputUrl);
if (!before.equals(after)) {
  throw new Error(
    "dashboard/tailwind.css was stale and has been regenerated; review and commit it",
  );
}

console.log("dashboard compiled CSS is current");
