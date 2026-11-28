# arc-vrf: RFC 9381 ECVRF and BLS proof of possession (Stage 0, dormant)

The first Stage 0 PR of ARC's validator-decentralization plan ("fix the crypto
foundation; touches nothing live"). This crate is a library and nothing else:
no workspace crate imports it, no feature flag changes existing behaviour, and
`arc-crypto::vrf` and `arc-crypto::bls` are untouched. A later PR wires the
primitives into leader election and key registration; a CI job in
`.github/workflows/stage0-arc-vrf.yml` fails if anything outside this
directory starts depending on the crate before then.

## What it contains

| Module | Primitive | Standard | Backend |
|---|---|---|---|
| `ecvrf` | Unique-output VRF: keygen, `prove`, `verify`, `proof_to_hash` | RFC 9381, ciphersuite `ECVRF-EDWARDS25519-SHA512-TAI` (`suite_string = 0x03`) | `curve25519-dalek` 4.1.3 + `sha2` 0.10.9 |
| `leader` | The byte layout of the VRF input `alpha` for leader election, committee sampling and epoch-seed contributions | ARC-specific, documented in the module | none |
| `bls_pop` | BLS12-381 key generation, proof of possession (`PopProve`/`PopVerify`), an `EnrolledBlsKey` type that exists only after the proof verified, aggregation of enrolled keys | IETF `draft-irtf-cfrg-bls-signature` PoP scheme, `min_pk`; RFC 9380 hash-to-curve | `blst` 0.3.17 |

Sizes: ECVRF secret key 32 bytes, public key 32, proof 80 (`Gamma || c || s`),
output 64. BLS public key 48, signature and proof of possession 96.

### The VRF input for leader election

```text
alpha = len(domain) as u8 || domain || chain_id (32) || epoch (u64 LE) || slot (u64 LE) || epoch_seed (32)
domain = "ARC-ECVRF-EDWARDS25519-SHA512-TAI/<purpose>/v1"
```

Fixed-width fields after a length-prefixed tag make the encoding injective; a
distinct tag per purpose means a leader-election proof can never be replayed
as a committee-sampling proof; naming the ciphersuite in the tag means a future
suite change changes every input. The public key is the try-and-increment salt
(RFC 9381 Section 5.5), so validators evaluating the same `alpha` hash
different curve points. `epoch_seed` is the seed fixed before the epoch starts
(lookahead), per `VALIDATOR-DECENTRALIZATION.md` Section 4.4. How `beta`
becomes a stake-weighted decision is the wiring PR's job, not this crate's.

## Why the ECVRF is composed here instead of taken from a crate

The task asked for "an audited, maintained RFC 9381 crate". On 7 October 2026
there is none on crates.io, so this crate composes the RFC over two libraries
that the workspace already locks and trusts, and documents the gap honestly.
Survey (crates.io metadata and repository trees, read on 2026-10-07):

| Crate | Latest release | What it is | Why not |
|---|---|---|---|
| [`ark-vrf`](https://crates.io/crates/ark-vrf) 0.5.3 | 2026-08-18, MIT | Tiny/Thin/Pedersen/Ring VRFs over arkworks, from [davxy/ark-vrf](https://github.com/davxy/ark-vrf) | The README describes the Tiny VRF as "loosely inspired by RFC-9381, adapted with a transcript-based Fiat-Shamir transform"; the source tree has no RFC 9381 module and its vectors are its own (`ed25519_sha-512_tai_tiny.json`), so it is not byte-compatible with RFC 9381 Appendix B. No audit statement. Pulls in the arkworks stack (`ark-ec`, `ark-ff`, `ark-std`, `ark-serialize` 0.6 and curve crates), whose field arithmetic is not constant-time. |
| [`vrf`](https://crates.io/crates/vrf) 0.2.5 | 2025-06-17, MIT | [witnet/vrf-rs](https://github.com/witnet/vrf-rs) | Implements "VRF-draft-05" (not RFC 9381), only P-256, K-163 and secp256k1, over OpenSSL; README: "This is experimental software. Be careful!" |
| [`ecvrf`](https://crates.io/crates/ecvrf) 0.4.4 | 2023-02-09 | "A curve25519+SHA3 verifiable random function" | Non-standard suite, no RFC 9381 vectors, no release since 2023. |
| [`vrf-r255`](https://crates.io/crates/vrf-r255) 0.1.0 | 2024-03-26 | ristretto255 VRF | Not an RFC 9381 ciphersuite. |
| [`fastcrypto`](https://crates.io/crates/fastcrypto) 0.1.11 | 2026-07-02 | Mysten Labs' library; its ECVRF is over ristretto255 | Not an RFC 9381 ciphersuite; very large dependency footprint. |
| [`ecvrf-rs`](https://crates.io/crates/ecvrf-rs) 1.0.0 | 2022-08-21 | "Elliptic Curve VRF implemented in Rust" | One release, about 4,000 downloads, no activity since 2022. |
| [`schnorrkel`](https://crates.io/crates/schnorrkel) 0.11.5 | 2025-07-14 | sr25519 signatures and VRF over ristretto | Its own VRF design (Merlin transcripts), not RFC 9381. |

What the composition is: `src/ecvrf.rs` is about 250 lines that call RFC 9381
Section 5 step by step (`encode_to_curve_try_and_increment`,
`nonce_generation_RFC8032`, `challenge_generation`, `decode_proof`,
`validate_key`, `proof_to_hash`) with every group operation, scalar operation
and point decoding done by `curve25519-dalek`, and every hash by `sha2`. The
only logic that is ARC's own is the order of the hash inputs and the
byte-exact checks that the RFC's three Appendix B.3 vectors pin down,
including the intermediate values (`x`, `H`, `ctr`, `k_string`, `k`, `U`, `V`).

What mitigates the absence of an audited ECVRF crate:

1. Conformance: all three RFC 9381 Appendix B.3 vectors pass byte-exactly, at
   every intermediate step, in CI on Linux and Windows.
2. The hard parts (constant-time scalar multiplication, point decompression,
   scalar reduction) are `curve25519-dalek`'s, which has a public audit (below)
   and already carries every Ed25519 signature on the ARC network.
3. Adversarial tests: single-bit flips over the whole proof, non-canonical
   scalar and point encodings, torsion-shifted `Gamma`, wrong key, wrong
   input, cross-ciphersuite confusion, the eight small-order public keys.
4. Strict decoding beyond what the RFC spells out: `curve25519-dalek` 4.1.3
   accepts a non-canonical `y` in `CompressedEdwardsY::decompress`, so the
   crate re-encodes every decoded point and requires a byte-exact round trip
   (RFC 8032 Section 5.1.3 semantics). `s >= q` is rejected as the RFC
   requires. `validate_key` always runs (the RFC lets an implementation
   support only that option).
5. External review by Astra before anything is wired in, and a small enough
   surface that a later swap to an audited crate, should one appear, changes
   only the module internals.

## Dependencies and audits

Exact versions are pinned in `Cargo.toml` (`=4.1.3`, `=0.10.9`, `=0.3.17`) and
are the versions `Cargo.lock` already held: this PR adds no package to the
lockfile, only the `arc-vrf` entry itself. "Audited" below means what the
public record supports, nothing more.

| Crate | Version (release date) | Licence | Audit record | Caveat |
|---|---|---|---|---|
| [`curve25519-dalek`](https://crates.io/crates/curve25519-dalek) | 4.1.3 (2024-06-18) | BSD-3-Clause | Quarkslab, "Security evaluation of dalek-cryptography libraries", report 19-06-594-REP, published August 2019, commissioned by Tari Labs: 30 engineer-days covering the Edwards, Ristretto and Scalar modules and the u64 and AVX2 backends; "No critical vulnerability was found." [Blog post](https://blog.quarkslab.com/security-audit-of-dalek-libraries.html), [report PDF](https://blog.quarkslab.com/resources/2019-08-26-audit-dalek-libraries/19-06-594-REP.pdf). | The audit predates the 4.x line (2023 onwards); we found no public re-audit of 4.x. The crate is maintained by the dalek-cryptography organisation and is the Ed25519 backend (via `ed25519-dalek`) of this workspace. |
| [`sha2`](https://crates.io/crates/sha2) | 0.10.9 (2025-04-30) | MIT OR Apache-2.0 | We found no formal audit of RustCrypto's `sha2`. | Already a dependency of `arc-crypto`; SHA-512 is validated transitively by the RFC 9381 vectors passing. |
| [`blst`](https://crates.io/crates/blst) | 0.3.17 (2026-07-24) | Apache-2.0 | README: "An initial audit of this library was conducted by NCC Group in January 2021" ([report PDF](https://research.nccgroup.com/wp-content/uploads/2021/01/NCC_Group_EthereumFoundation_ETHF002_Report_2021-01-20_v1.0.pdf)); "Formal verification of this library by Galois is on-going" ([GaloisInc/BLST-Verification](https://github.com/GaloisInc/BLST-Verification)). The README states the library implements the IETF BLS signature draft and RFC 9380 hashing to elliptic curves and is "under active development". | The 2021 audit covered the code of that time; 0.3.17 is five years of releases later. `blst` is already `arc-crypto::bls`'s library and the BLS library of the Ethereum consensus clients. |

Supporting crates (`zeroize`, `rand_core`, `thiserror`; dev: `hex`, `rand`,
`serde_json`) come from the workspace dependency table at their locked versions.

## Tests

Everything runs in CI (`cargo test -p arc-vrf`) on `ubuntu-latest` and
`windows-latest`; nothing is built on the shared development Mac. Vector files
live in `tests/vectors/` and were generated by a script from the downloaded
sources, not retyped; each file records its source URL (and for the RFC, the
SHA-256 of the text).

| Area | Test | Data |
|---|---|---|
| RFC 9381 conformance | `ecvrf::tests::rfc9381_appendix_b3_vectors_are_reproduced_byte_exactly`: Examples 16, 17, 18; checks `PK`, `x`, `H`, `ctr`, `k_string`, `k`, `U`, `V`, `pi`, `beta`, `proof_to_hash`, and the byte round trip | `rfc9381_b3_ecvrf_edwards25519_sha512_tai.json` |
| Uniqueness | same `(pk, alpha)` always yields the same `beta`; `prove` is deterministic; different keys or inputs give different outputs | `ecvrf_adversarial.rs` |
| Forgery | every one of the 640 single-bit flips of a proof is rejected (at decoding or at verification) | `ecvrf_adversarial.rs` |
| Malleability | `s + q` rejected; non-canonical `Gamma` (`y + p`, and `x = 0` with the sign bit) rejected; `Gamma + T` for each of the seven non-trivial torsion points rejected while `proof_to_hash` stays equal (cofactor clearing) | `ecvrf_adversarial.rs` |
| Wrong key / input | other public key, altered input, extended input, empty input | `ecvrf_adversarial.rs` |
| Cross-ciphersuite | a proof made under `suite_string = 0x04` never verifies under `0x03`, and the outputs differ | `ecvrf::tests::cross_ciphersuite_confusion_is_rejected` |
| Key validation | the eight small-order points of edwards25519 (RFC 9381 Section 5.4.5 list), both sign bits, plus `p` and `p + 1` and a non-curve encoding | `edwards25519_small_order_points.json` |
| Leader-election input | layout, injectivity, purpose separation; a proof for one purpose/epoch/slot/chain/seed never verifies for another | `leader::tests`, `ecvrf_adversarial.rs` |
| BLS key generation | ERC-2333 test cases 0 to 3 (`derive_master_SK` is IETF `KeyGen` with empty `key_info`, which `blst_keygen` implements as draft version 4); short IKM rejected; zero and out-of-range scalars rejected | `erc2333_keygen.json` |
| BLS signature plumbing | ethereum/bls12-381-tests v0.1.2: `sign` (including the zero key), `verify` (valid, tampered, infinity), all `deserialization_G1` cases | `ethereum_bls12_381_tests.json` |
| Proof of possession | valid proof verifies and enrolls; proof from another key fails; a message signature over the key bytes is not a proof and a proof is not a message signature (distinct DSTs); single-bit flips rejected; identity and malformed keys and signatures rejected | `bls_pop.rs` |
| Rogue-key attack | `pk_rogue = pk_attacker - pk_victim` is built with `blst`; the attack is shown to work against naive aggregation; every proof of possession the attacker can produce for `pk_rogue` fails, `EnrolledBlsKey::enroll` refuses it, honest enrolment and aggregation still verify | `bls_pop.rs` |
| ARC regression vectors | `committed_regression_vectors_reproduce`: ARC's own vectors (12 leader-election / committee / epoch-seed inputs with proofs and outputs for three fixed test keys; 3 BLS key-generation, proof-of-possession and signature cases) must reproduce exactly, and the ignored `print_vectors` printer must agree with the file. The file names the CI job that printed it | `arc_regression_vectors.json` (generated by the `arc-vrf test (ubuntu-latest)` job of run 37559077135) |

## What this PR does not do

- It does not wire anything into consensus, the node, the RPC or the wallet.
  `arc-node/src/vrf.rs` and `arc-crypto/src/vrf.rs` are unchanged; the
  signature-hash VRF they contain is replaced only when the wiring PR removes
  its users.
- It does not implement `ECVRF-EDWARDS25519-SHA512-ELL2` (constant-time
  hash-to-curve). TAI is the right fit for public inputs; ELL2 would need
  field-element access that `curve25519-dalek` does not expose, or another
  dependency.
- It does not define the lottery (how `beta` is compared to stake), the epoch
  seed protocol, BLS key enrolment records, or any wire format.
- It does not claim an audit of the ECVRF composition. That is Astra's review
  and, later, the external consensus review the plan already calls for.

## Follow-ups (Stage 0 wiring, separate PRs)

1. Epoch seed: fix the seed for epoch `e + 2` at the end of epoch `e`; interim
   source is a hash of handover-certificate ECVRF contributions
   (`Purpose::EpochSeedContribution`), target is a threshold-BLS beacon.
2. Leader schedule: deterministic, stake-weighted, derived from the epoch seed
   via `VrfInput { purpose: LeaderElection, .. }`; replace the address-bytes
   "pubkey placeholder" selector in `arc-node/src/vrf.rs`.
3. Key registration: a validator-set-change record carries the BLS public key
   and its proof of possession, bound to the Ed25519 identity; record
   validation rejects any key without a valid proof, any duplicate and the
   identity; only `EnrolledBlsKey` reaches aggregation.
4. Remove `arc-crypto::vrf` and the `vrf_approved = true` bypass once nothing
   uses them.
5. If an audited RFC 9381 crate appears, swap the internals of `ecvrf.rs` and
   keep the tests.

## References

- RFC 9381, Verifiable Random Functions (VRFs): <https://www.rfc-editor.org/rfc/rfc9381>
- RFC 8032, Edwards-Curve Digital Signature Algorithm (EdDSA): <https://www.rfc-editor.org/rfc/rfc8032>
- RFC 9380, Hashing to Elliptic Curves: <https://www.rfc-editor.org/rfc/rfc9380>
- IETF BLS signature draft: <https://datatracker.ietf.org/doc/draft-irtf-cfrg-bls-signature/>
- ERC-2333, BLS12-381 Key Generation: <https://github.com/ethereum/ERCs/blob/master/ERCS/erc-2333.md>
- ethereum/bls12-381-tests v0.1.2: <https://github.com/ethereum/bls12-381-tests/releases/tag/v0.1.2>
