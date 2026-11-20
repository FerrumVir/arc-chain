#!/usr/bin/env node

import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";

const EXPECTED_SHA256 = "32bb64162a3be8ebb34e0d0b3ea3fef6e1a3d91e2c1375fec2bb2dc1e72e21c6";

const cssUrl = new URL("./tailwind.css", import.meta.url);
const appUrl = new URL("./app.css", import.meta.url);
const bytes = readFileSync(cssUrl);
const css = bytes.toString("utf8");
const app = readFileSync(appUrl, "utf8");
const failures = [];

const digest = createHash("sha256").update(bytes).digest("hex");
if (digest !== EXPECTED_SHA256) {
  failures.push(
    `tailwind.css changed (sha256 ${digest}); review the change and update EXPECTED_SHA256 in verify-css.mjs`,
  );
}

const withoutComments = (text) => text.replace(/\/\*[\s\S]*?\*\//g, "");
const cssCode = withoutComments(css);
for (const directive of ["@tailwind", "@apply", "@config", "@import", "theme("]) {
  if (cssCode.includes(directive)) {
    failures.push(`tailwind.css contains the build-time directive ${directive}`);
  }
}

const urls = [...cssCode.matchAll(/url\(\s*(['"]?)([^'")]+)\1\s*\)/g)].map((match) => match[2]);
if (urls.length === 0) {
  failures.push("tailwind.css declares no self-hosted fonts");
}
for (const url of urls) {
  if (!/^(\.\/)?fonts\/[A-Za-z0-9._-]+\.woff2$/.test(url)) {
    failures.push(`tailwind.css references ${url}, which is not a local fonts/*.woff2 file`);
  } else if (!existsSync(new URL(`./${url.replace(/^\.\//, "")}`, import.meta.url))) {
    failures.push(`tailwind.css references ${url}, which does not exist`);
  }
}

const appCode = withoutComments(app);
const defined = new Set(
  [...`${cssCode}\n${appCode}`.matchAll(/(--[A-Za-z0-9_-]+)\s*:/g)].map((match) => match[1]),
);
const used = new Set(
  [...`${cssCode}\n${appCode}`.matchAll(/var\(\s*(--[A-Za-z0-9_-]+)/g)].map((match) => match[1]),
);
for (const name of used) {
  if (!defined.has(name)) failures.push(`custom property ${name} is used but never defined`);
}

const opened = cssCode.split("{").length - 1;
const closed = cssCode.split("}").length - 1;
if (opened !== closed) {
  failures.push(`tailwind.css has unbalanced braces (${opened} "{" vs ${closed} "}")`);
}

if (failures.length > 0) {
  for (const failure of failures) console.error(`verify-css: ${failure}`);
  process.exit(1);
}

console.log(
  `dashboard static CSS verified: sha256 ${digest}, ${urls.length} local fonts, ${defined.size} custom properties`,
);
