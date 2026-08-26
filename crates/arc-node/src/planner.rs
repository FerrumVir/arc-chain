//! Milestone D (#38): deterministic shard-assignment planner.
//!
//! Takes a snapshot of open model requests, advertised capacity, and
//! current coverage, and computes which node should hold which layer
//! range for the next epoch. The output is the body of a
//! `ShardAssignmentProposal` tx that any full node can replay.
//!
//! **Determinism** is the whole point - given the exact same inputs, two
//! nodes running this function on different machines must produce
//! byte-identical output, or the proposal broadcast loses consensus
//! immediately. That means:
//!
//! - All inputs are sorted by stable keys (model_id, node_pubkey) before
//!   iteration.
//! - No `HashMap` iteration in the algorithm - `BTreeMap` only.
//! - No randomness. No floating-point. No wall clocks.
//!
//! # MVP algorithm
//!
//! For each open request, sorted by (model_id, request_id):
//!   1. Compute the layer ranges this model needs, partitioning `[0,
//!      n_layers)` into fixed-size buckets so any worker can serve a
//!      whole bucket.
//!   2. For each range, pick the top `target_k_replication` capacity
//!      ads with the most free RAM that aren't already serving this
//!      range. Ties broken by `node_pubkey` lexicographic order.
//!   3. Record the assignment entries. Track per-node remaining RAM so
//!      a single worker isn't assigned more than its advertised capacity.
//!
//! The MVP deliberately uses the simplest greedy rule; more nuanced
//! heuristics (cached-chunk stickiness for Milestone E, geographic
//! spread for D's acceptance criteria) are follow-ups on the same
//! signature.

use arc_crypto::Hash256;
use arc_types::transaction::{
    AssignmentEntry, CapacityAdvertisementBody, ModelRequestBody, ShardAssignmentProposalBody,
};
use std::collections::BTreeMap;

/// `(node_pubkey, model_id)` → the layer ranges that node is assigned for that
/// model. Raw `[u8; 32]` keys because `Hash256` isn't `Ord`; rewrapped on output.
type PerNodeModelRanges = BTreeMap<([u8; 32], [u8; 32]), Vec<(u32, u32)>>;

/// Per-model layer count snapshot. The planner doesn't itself know how
/// many layers a model has - callers fetch it from the registry and
/// feed the `(model_id, n_layers)` pairs in.
/// Model layer count snapshot keyed by raw bytes of the model_id. Uses
/// `[u8; 32]` instead of `Hash256` because `Hash256` doesn't implement
/// `Ord` - the raw array does.
pub type LayerCounts = BTreeMap<[u8; 32], u32>;

/// Fixed layer-range bucket size. A 32-layer 7B fits in 6 ranges of 5-6
/// layers each; an 80-layer 70B fits in 16 ranges of 5 layers each.
/// Same size across all models keeps bookkeeping simple; tuning per
/// model is a follow-up.
pub const DEFAULT_BUCKET_LAYERS: u32 = 5;

/// Compute an assignment proposal deterministically. Mutations are kept
/// inside the function so the returned `ShardAssignmentProposalBody` is
/// pure output.
///
/// Returns `None` if there are no open requests or no advertised
/// capacity - nothing meaningful to propose.
pub fn compute_assignment(
    open_requests: &[ModelRequestBody],
    capacity_ads: &[CapacityAdvertisementBody],
    layer_counts: &LayerCounts,
    epoch_blocks: u64,
) -> Option<ShardAssignmentProposalBody> {
    if open_requests.is_empty() || capacity_ads.is_empty() {
        return None;
    }

    // Sort requests by (model_id, request_id) for determinism.
    let mut reqs: Vec<&ModelRequestBody> = open_requests.iter().collect();
    reqs.sort_by(|a, b| {
        a.model_id
            .0
            .cmp(&b.model_id.0)
            .then(a.request_id.cmp(&b.request_id))
    });

    // Per-node remaining RAM, keyed by node_pubkey sorted ascending.
    let mut remaining: BTreeMap<[u8; 32], u64> = capacity_ads
        .iter()
        .map(|c| (c.node_pubkey, c.ram_bytes))
        .collect();
    // ad_by_pubkey keeps auxiliary fields (region, vram) that tie-break
    // or inform future planner heuristics.
    let ad_by_pubkey: BTreeMap<[u8; 32], &CapacityAdvertisementBody> =
        capacity_ads.iter().map(|c| (c.node_pubkey, c)).collect();

    // (node_pubkey, model_id_bytes) -> Vec<(start, end)>. Collect all
    // assignments keyed by (node, model) so we can emit one
    // AssignmentEntry per pair at the end. Uses raw [u8; 32] for
    // model_id because Hash256 isn't Ord; we rewrap at output time.
    let mut per_node_model: PerNodeModelRanges = BTreeMap::new();

    // Rough per-range memory bid: assume a layer costs ~100 MB at INT16
    // for a 7B model; bucket cost = layers_in_bucket × 100 MB. This is
    // a coarse heuristic - the real chain would pull per-model costs
    // from the registry, but it's good enough to prevent over-assigning
    // a 1 GB-RAM laptop to 8 ranges of a 70B model.
    const BYTES_PER_LAYER_HEURISTIC: u64 = 100 * 1024 * 1024;

    // For each request, for each range, pick k replicas.
    for req in reqs {
        let n_layers = *layer_counts.get(&req.model_id.0).unwrap_or(&32);
        let k = req.target_k_replication.max(1);
        let ranges = layer_ranges(n_layers, DEFAULT_BUCKET_LAYERS);

        for (start, end) in ranges {
            let bucket_bytes = BYTES_PER_LAYER_HEURISTIC * (end - start) as u64;

            // Candidate list: every node that has enough remaining RAM.
            // Sort by (remaining_ram DESC, node_pubkey ASC) - deterministic
            // and biases toward high-capacity nodes so planner finishes
            // faster when supply is abundant.
            let mut candidates: Vec<([u8; 32], u64)> = remaining
                .iter()
                .filter(|(_, ram)| **ram >= bucket_bytes)
                .map(|(pk, ram)| (*pk, *ram))
                .collect();
            candidates.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

            let picked: Vec<[u8; 32]> = candidates
                .iter()
                .take(k as usize)
                .map(|(pk, _)| *pk)
                .collect();
            for pk in &picked {
                *remaining.get_mut(pk).unwrap() -= bucket_bytes;
                per_node_model
                    .entry((*pk, req.model_id.0))
                    .or_default()
                    .push((start, end));
            }
        }
    }

    let mut assignments: Vec<AssignmentEntry> = per_node_model
        .into_iter()
        .map(|((pk, mid_bytes), ranges)| AssignmentEntry {
            node_pubkey: pk,
            model_id: Hash256(mid_bytes),
            ranges,
        })
        .collect();
    // BTreeMap iteration above is already sorted by (pk, mid_bytes);
    // the explicit sort here is cheap insurance that downstream changes
    // to the collection don't silently produce a different serialization
    // order and break determinism.
    assignments.sort_by(|a, b| {
        a.node_pubkey
            .cmp(&b.node_pubkey)
            .then(a.model_id.0.cmp(&b.model_id.0))
    });

    if assignments.is_empty() {
        return None;
    }

    // input_snapshot_hash commits to every input in a canonical
    // serialization so the proposer's claim is auditable. Any node
    // recomputing from the same on-chain state must arrive at the
    // same hash.
    let input_snapshot_hash =
        compute_input_snapshot_hash(open_requests, capacity_ads, layer_counts);

    // ad_by_pubkey is consumed into the region-spread post-pass
    // (follow-up); kept live to avoid a "warning: unused variable" at
    // this size of MVP.
    let _ = ad_by_pubkey;

    Some(ShardAssignmentProposalBody {
        epoch_blocks,
        assignments,
        input_snapshot_hash,
    })
}

/// Partition `[0, n_layers)` into contiguous buckets of `bucket` layers.
/// Last bucket may be smaller if `n_layers` isn't a multiple of `bucket`.
pub fn layer_ranges(n_layers: u32, bucket: u32) -> Vec<(u32, u32)> {
    let bucket = bucket.max(1);
    let mut out = Vec::new();
    let mut s = 0u32;
    while s < n_layers {
        let e = (s + bucket).min(n_layers);
        out.push((s, e));
        s = e;
    }
    out
}

/// Canonical-bytes hash of every planner input. Sorts each input list
/// before hashing so the order the caller built the vectors doesn't
/// affect the output.
pub fn compute_input_snapshot_hash(
    open_requests: &[ModelRequestBody],
    capacity_ads: &[CapacityAdvertisementBody],
    layer_counts: &LayerCounts,
) -> Hash256 {
    let mut reqs: Vec<&ModelRequestBody> = open_requests.iter().collect();
    reqs.sort_by(|a, b| {
        a.model_id
            .0
            .cmp(&b.model_id.0)
            .then(a.request_id.cmp(&b.request_id))
    });
    let mut ads: Vec<&CapacityAdvertisementBody> = capacity_ads.iter().collect();
    ads.sort_by_key(|a| a.node_pubkey);

    let mut buf = Vec::new();
    for r in &reqs {
        buf.extend_from_slice(&r.model_id.0);
        buf.extend_from_slice(&r.request_id);
        buf.extend_from_slice(&r.target_k_replication.to_le_bytes());
        buf.extend_from_slice(&r.bond_per_layer_epoch.to_le_bytes());
        buf.extend_from_slice(&r.max_wait_secs.to_le_bytes());
    }
    for a in &ads {
        buf.extend_from_slice(&a.node_pubkey);
        buf.extend_from_slice(&a.ram_bytes.to_le_bytes());
        buf.extend_from_slice(&a.vram_bytes.to_le_bytes());
        buf.extend_from_slice(&a.bandwidth_mbps.to_le_bytes());
        buf.extend_from_slice(&a.uptime_hint_mins.to_le_bytes());
        buf.extend_from_slice(&a.stake.to_le_bytes());
        buf.extend_from_slice(&(a.region.len() as u32).to_le_bytes());
        buf.extend_from_slice(a.region.as_bytes());
    }
    for (mid, n) in layer_counts {
        buf.extend_from_slice(mid);
        buf.extend_from_slice(&n.to_le_bytes());
    }
    arc_crypto::hash_bytes(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    fn req(model_tag: &[u8], tag: &[u8], k: u32) -> ModelRequestBody {
        ModelRequestBody {
            request_id: hash_bytes(tag).0,
            model_id: hash_bytes(model_tag),
            target_k_replication: k,
            bond_per_layer_epoch: 100,
            max_wait_secs: 300,
        }
    }

    fn ad(pk: u8, ram_gb: u64, region: &str) -> CapacityAdvertisementBody {
        CapacityAdvertisementBody {
            node_pubkey: [pk; 32],
            ram_bytes: ram_gb * 1024 * 1024 * 1024,
            vram_bytes: 0,
            bandwidth_mbps: 100,
            uptime_hint_mins: 1440,
            stake: 5_000_000,
            region: region.into(),
        }
    }

    #[test]
    fn test_layer_ranges_exact_split() {
        assert_eq!(layer_ranges(10, 5), vec![(0, 5), (5, 10)]);
    }

    #[test]
    fn test_layer_ranges_trailing_remainder() {
        assert_eq!(layer_ranges(12, 5), vec![(0, 5), (5, 10), (10, 12)]);
    }

    #[test]
    fn test_compute_assignment_empty_inputs_returns_none() {
        let ads = vec![ad(1, 16, "US")];
        let counts = LayerCounts::new();
        assert!(compute_assignment(&[], &ads, &counts, 100).is_none());

        let reqs = vec![req(b"m", b"r1", 1)];
        assert!(compute_assignment(&reqs, &[], &counts, 100).is_none());
    }

    #[test]
    fn test_compute_assignment_is_deterministic() {
        // Shuffle the input orders across two calls; expect same output.
        let m = b"llama-7b";
        let r1 = req(m, b"r1", 2);
        let r2 = req(m, b"r2", 1);
        let a1 = ad(1, 32, "US");
        let a2 = ad(2, 16, "EU");
        let a3 = ad(3, 8, "AS");
        let counts: LayerCounts = [(hash_bytes(m).0, 10u32)].into_iter().collect();

        let p1 = compute_assignment(
            &[r1.clone(), r2.clone()],
            &[a1.clone(), a2.clone(), a3.clone()],
            &counts,
            1000,
        )
        .unwrap();
        let p2 = compute_assignment(&[r2, r1], &[a3, a1, a2], &counts, 1000).unwrap();

        assert_eq!(p1.assignments.len(), p2.assignments.len());
        for (x, y) in p1.assignments.iter().zip(&p2.assignments) {
            assert_eq!(x.node_pubkey, y.node_pubkey);
            assert_eq!(x.model_id, y.model_id);
            assert_eq!(x.ranges, y.ranges);
        }
        assert_eq!(p1.input_snapshot_hash, p2.input_snapshot_hash);
    }

    #[test]
    fn test_compute_assignment_honors_k_replication() {
        // k=3 on a single 10-layer model with 5 candidate workers should
        // produce exactly 3 replicas per range (2 ranges × 3 = 6
        // assignment slots across nodes).
        let m = b"m7b";
        let reqs = vec![req(m, b"r1", 3)];
        let ads = vec![
            ad(1, 100, "US"),
            ad(2, 100, "US"),
            ad(3, 100, "EU"),
            ad(4, 100, "EU"),
            ad(5, 100, "AS"),
        ];
        let counts: LayerCounts = [(hash_bytes(m).0, 10u32)].into_iter().collect();
        let p = compute_assignment(&reqs, &ads, &counts, 1000).unwrap();

        // 2 ranges × 3 replicas each = 6 range-assignments total,
        // distributed across distinct (node, model) entries.
        let total_ranges: usize = p.assignments.iter().map(|a| a.ranges.len()).sum();
        assert_eq!(total_ranges, 6);
        // Exactly 3 distinct node_pubkeys per range - check first range.
        let servers_of_0_5: Vec<[u8; 32]> = p
            .assignments
            .iter()
            .filter(|a| a.ranges.iter().any(|(s, e)| *s == 0 && *e == 5))
            .map(|a| a.node_pubkey)
            .collect();
        assert_eq!(servers_of_0_5.len(), 3);
    }

    #[test]
    fn test_compute_assignment_skips_underresourced_nodes() {
        // 1 GB laptop can hold ~2 ranges of a 10-layer model at
        // 100 MB/layer × 5 layers = 500 MB per bucket. On the 3rd
        // bucket it should be skipped.
        let m = b"m7b";
        let reqs = vec![req(m, b"r1", 1)];
        // Only 1 node with 1 GB - enough for 2 ranges. A 3rd-range
        // request would need another node (none available) so the
        // proposal just omits it.
        let ads = vec![ad(1, 1, "US")];
        let counts: LayerCounts = [(hash_bytes(m).0, 15u32)].into_iter().collect();
        let p = compute_assignment(&reqs, &ads, &counts, 1000).unwrap();
        // 15 layers / 5 per bucket = 3 ranges, but node can hold
        // only 2 × 500 MB = 1000 MB.
        let entry = p
            .assignments
            .iter()
            .find(|a| a.node_pubkey == [1u8; 32])
            .unwrap();
        assert!(entry.ranges.len() <= 2);
    }
}
