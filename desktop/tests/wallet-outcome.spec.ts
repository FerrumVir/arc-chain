import { expect, test } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { classifyWalletFailure } from "../src/lib/wallet-outcome";

// Pure logic: no page. Which failed claims and transfers may be told "didn't
// go through", and which must say to check the balance before sending again
// because the host may have accepted the transaction.

const __dirnameHere = path.dirname(fileURLToPath(import.meta.url));
const rustSource = (file: string) =>
  fs.readFileSync(path.resolve(__dirnameHere, "..", "src-tauri", "src", file), "utf8");

test.describe("wallet failure wording", () => {
  test("refusals before or by the host read as refused; a lost answer after the POST never does", () => {
    // Stopped before anything was signed (commands.rs send_arc_inner, wallet.rs).
    for (const refused of [
      "insufficient balance: available 0 ARC, requested 1.25 ARC plus 0.000000001 ARC network fee",
      "amount must be greater than zero",
      "amount must be a plain positive decimal with at most 9 decimal places",
      "enter an ARC amount",
      "ARC supports at most 9 decimal places; the amount was not rounded",
      "no identity",
      "this chain carries only native paid-inference transactions, so a transfer can never be included; nothing was signed",
      "wallet RPC must use HTTPS (or HTTP on loopback for local dev)",
      "HTTP 503",
      "account response is missing an exact u64 balance",
      "could not read transaction domain from https://140.82.16.112: error sending request for url (https://140.82.16.112/network/info)",
      // The host answered, and said no (rpc_client.rs submit_signed_transfer, faucet_claim).
      "transfer was rejected by https://140.82.16.112 (HTTP 400): nonce too low",
      "https://140.82.16.112 did not acknowledge the signed transfer as pending",
      "https://140.82.16.112 acknowledged a different transaction hash; refusing to track it",
      "faucet exhausted (429)",
      "claim failed (503)",
      "faucet did not acknowledge the claim as pending",
      "a faucet claim is an ordinary transfer, which this chain cannot include",
    ]) {
      expect(classifyWalletFailure(refused), refused).toBe("refused");
      expect(classifyWalletFailure(new Error(refused)), refused).toBe("refused");
    }

    // The POST went out and the answer was lost or unusable: the host may have mined it.
    for (const unconfirmed of [
      "transfer submission failed: error sending request for url (https://140.82.16.112/tx/submit_signed): operation timed out",
      "transfer submission failed: error sending request for url (https://140.82.16.112/tx/submit_signed): connection reset by peer",
      "invalid transfer submission response from https://140.82.16.112: expected value at line 1 column 1",
      "https://140.82.16.112 omitted tx_hash from the submission response",
      "error sending request for url (https://140.82.16.112/faucet/claim): operation timed out",
      "error decoding response body",
      "faucet accepted the request but returned no valid transaction hash",
      "faucet response omitted its exact base-unit amount",
      // Anything unrecognised lands on the safe side.
      "something nobody anticipated",
      "",
    ]) {
      expect(classifyWalletFailure(unconfirmed), unconfirmed).toBe("unconfirmed");
    }
    expect(classifyWalletFailure(undefined)).toBe("unconfirmed");
  });

  test("the native wording it recognises is the wording the Rust side writes", () => {
    const rpcClient = rustSource("rpc_client.rs");
    const commands = rustSource("commands.rs");
    // Post-POST failures, which must never read as "didn't go through".
    expect(rpcClient).toContain('"transfer submission failed: {e}"');
    expect(rpcClient).toContain('"invalid transfer submission response from {base_url}: {e}"');
    expect(rpcClient).toContain("omitted tx_hash from the submission response");
    expect(rpcClient).toContain('"faucet accepted the request but returned no valid transaction hash"');
    expect(rpcClient).toContain('"faucet response omitted its exact base-unit amount"');
    // Refusals the host answered with.
    expect(rpcClient).toContain('"transfer was rejected by {} (HTTP {}): {}"');
    expect(rpcClient).toContain('"{} did not acknowledge the signed transfer as pending"');
    expect(rpcClient).toContain('"{} acknowledged a different transaction hash; refusing to track it"');
    expect(rpcClient).toContain('"faucet did not acknowledge the claim as pending"');
    expect(rpcClient).toContain('format!("{} ({})", err, status)');
    // Refusals before the POST.
    expect(commands).toContain('"insufficient balance: available {} ARC');
    expect(commands).toContain('"amount must be greater than zero"');
    expect(commands).toContain('"no identity"');
    expect(commands).toContain("this chain carries only native paid-inference transactions");
  });
});
