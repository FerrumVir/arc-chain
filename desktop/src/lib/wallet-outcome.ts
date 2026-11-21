export type WalletFailureKind = "refused" | "unconfirmed";

const REFUSED: readonly RegExp[] = [
  /^insufficient balance: /,
  /^amount (?:must|plus|contains)/,
  /^enter (?:an ARC amount|digits)/,
  /^ARC supports at most 9 decimal places/,
  /^no identity$/,
  /^this chain carries only native paid-inference transactions/,
  /^selected host returned an invalid wallet balance$/,
  /^(?:remote )?wallet RPC /,
  /^HTTP \d{3}$/,
  /^account response is missing /,
  /^could not read transaction domain from /,
  /^invalid \/network\/info response from /,
  /^transfer was rejected by /,
  / did not acknowledge the signed transfer as pending$/,
  / acknowledged a different transaction hash; refusing to track it$/,
  / \(\d{3}\)$/,
  /^faucet did not acknowledge the claim as pending$/,
  /^a faucet claim is an ordinary transfer/,
];

export function classifyWalletFailure(error: unknown): WalletFailureKind {
  const text = (error instanceof Error ? error.message : String(error ?? "")).trim();
  return REFUSED.some((pattern) => pattern.test(text)) ? "refused" : "unconfirmed";
}
