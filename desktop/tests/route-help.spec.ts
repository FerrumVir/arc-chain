import { expect, test } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

// Pure: no page. Ask ARC's "How this works" copy must describe the route the
// native code takes. From at least v0.8.9 to v0.8.16 it said "Your prompt goes
// to your own node first", while run_inference_local_inner skipped the local
// node whenever the session's chain origin was a remote coordinator, which is
// the usual case. #223 corrected the copy; this ties the two together so
// either one changing alone fails here (ARC-410).

const __dirnameHere = path.dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => fs.readFileSync(path.resolve(__dirnameHere, rel), "utf8");

function localInnerBody(commands: string): string {
  const start = commands.indexOf("async fn run_inference_local_inner(");
  expect(start).toBeGreaterThan(-1);
  const end = commands.indexOf("\nasync fn ", start + 10);
  return commands.slice(start, end === -1 ? undefined : end);
}

const flat = (s: string) => s.replace(/\s+/g, " ").replace(/\{" "\}/g, " ").replace(/&rsquo;/g, "'");

test.describe("Ask ARC route help matches the native route", () => {
  test("native: a remote session origin skips the local node before anything is sent", () => {
    const body = localInnerBody(read("../src-tauri/src/commands.rs"));
    expect(body).toContain("established_chain_origin(state).await");
    expect(body).toMatch(/if origin != host \{\s*return Err\(inference_pre_dispatch_unavailable\(/);
    expect(body).toContain("immutable session chain origin is remote; local inference write was skipped");
  });

  test("copy: the coordinator goes first and the local node only when no coordinator has answered", () => {
    const inference = flat(read("../src/screens/Inference.tsx"));
    const help = /data-testid="inference-route-help">(.*?)<\/p>/.exec(inference)?.[1] ?? "";
    expect(help).toContain("Your prompt goes to the ARC coordinator this app reads the chain from");
    expect(help).toContain("takes the prompt only if no coordinator has answered the app yet this session");
    expect(help).toContain("or if the app reads the chain from your node");
    expect(inference).toContain("Who served it.</strong> A named community worker or coordinator on the ARC network, or your own node when it answered.");
  });

  test("no user-facing copy still says the prompt goes to your own node first", () => {
    const srcDir = path.resolve(__dirnameHere, "../src");
    const files: string[] = [];
    const walk = (dir: string) => {
      for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
        const full = path.join(dir, entry.name);
        if (entry.isDirectory()) walk(full);
        else if (/\.(tsx?|json)$/.test(entry.name)) files.push(full);
      }
    };
    walk(srcDir);
    for (const file of files) {
      const text = fs.readFileSync(file, "utf8");
      expect(/own node first/i.test(text), path.relative(srcDir, file)).toBe(false);
    }
  });
});
