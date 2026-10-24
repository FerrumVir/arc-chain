/**
 * What a failed faucet claim or transfer can honestly be told.
 *
 * The native path (commands.rs `send_arc_inner`, `faucet_claim`;
 * rpc_client.rs `submit_signed_transfer`, `faucet_claim`) fails in two ways
 * that need different words:
 *
 * - **refused**: it stopped before the POST (amount, identity, origin,
 *   balance, signing domain), or the host answered with a rejection (a
 *   non-2xx status, a body that did not acknowledge the transaction as
 *   pending, or a hash that is not ours). Nothing of ours can be included,
 *   so "didn't go through" is true and sending again is safe.
 * - **unconfirmed**: the POST went out and the answer was lost or unusable
 *   (a send error such as a timeout or a reset, or a 2xx body that could not
 *   be read or that omitted the hash). The host may have accepted and mined
 *   it. Saying "didn't go through" here invites a second, duplicate send, so
 *   the sentence asks the person to check their balance first.
 *
 * Anything this module does not recognise is treated as unconfirmed: that is
 * the safe side. wallet-outcome.spec.ts pins the native wording matched here
 * against the Rust sources.
 */
export type WalletFailureKind = "refused" | "unconfirmed";

const REFUSED: readonly RegExp[] = [
  // commands.rs send_arc_inner, before anything is signed.
  /^insufficient balance: /,
  /^amount (?:must|plus|contains)/,
  /^enter (?:an ARC amount|digits)/,
  /^ARC supports at most 9 decimal places/,
  /^no identity$/,
  /^this chain carries only native paid-inference transactions/,
  /^selected host returned an invalid wallet balance$/,
  // wallet.rs validate_rpc_origin.
  /^(?:remote )?wallet RPC /,
  // rpc_client.rs fetch_balance and transaction_signing_domain, both read before the POST.
  /^HTTP \d{3}$/,
  /^account response is missing /,
  /^could not read transaction domain from /,
  /^invalid \/network\/info response from /,
  // rpc_client.rs submit_signed_transfer: the host answered, and said no.
  /^transfer was rejected by /,
  / did not acknowledge the signed transfer as pending$/,
  / acknowledged a different transaction hash; refusing to track it$/,
  // rpc_client.rs faucet_claim: a non-2xx is "{error} ({status})"; a body that did not say pending.
  / \(\d{3}\)$/,
  /^faucet did not acknowledge the claim as pending$/,
  // A protocol-4 chain refuses the transfer type outright (commands.rs, native-paid preflight).
  /^a faucet claim is an ordinary transfer/,
];

/** Classify the error text a failed `send_arc` or `faucet_claim` returned. */
export function classifyWalletFailure(error: unknown): WalletFailureKind {
  const text = (error instanceof Error ? error.message : String(error ?? "")).trim();
  return REFUSED.some((pattern) => pattern.test(text)) ? "refused" : "unconfirmed";
}
