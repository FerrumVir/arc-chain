use arc_crypto::Hash256;
use arc_types::transaction::{
    AssignmentEntry, CapacityAdvertisementBody, ModelRequestBody, ShardAssignmentProposalBody,
};
use std::collections::BTreeMap;

type PerNodeModelRanges = BTreeMap<([u8; 32], [u8; 32]), Vec<(u32, u32)>>;

pub type LayerCounts = BTreeMap<[u8; 32], u32>;

pub const DEFAULT_BUCKET_LAYERS: u32 = 5;

pub fn compute_assignment(
    open_requests: &[ModelRequestBody],
    capacity_ads: &[CapacityAdvertisementBody],
    layer_counts: &LayerCounts,
    epoch_blocks: u64,
) -> Option<ShardAssignmentProposalBody> {
    if open_requests.is_empty() || capacity_ads.is_empty() {
        return None;
    }

    let mut reqs: Vec<&ModelRequestBody> = open_requests.iter().collect();
    reqs.sort_by(|a, b| {
        a.model_id
            .0
            .cmp(&b.model_id.0)
            .then(a.request_id.cmp(&b.request_id))
    });

    let mut remaining: BTreeMap<[u8; 32], u64> = capacity_ads
        .iter()
        .map(|c| (c.node_pubkey, c.ram_bytes))
        .collect();
    let ad_by_pubkey: BTreeMap<[u8; 32], &CapacityAdvertisementBody> =
        capacity_ads.iter().map(|c| (c.node_pubkey, c)).collect();

    let mut per_node_model: PerNodeModelRanges = BTreeMap::new();

    const BYTES_PER_LAYER_HEURISTIC: u64 = 100 * 1024 * 1024;

    for req in reqs {
        let n_layers = *layer_counts.get(&req.model_id.0).unwrap_or(&32);
        let k = req.target_k_replication.max(1);
        let ranges = layer_ranges(n_layers, DEFAULT_BUCKET_LAYERS);

        for (start, end) in ranges {
            let bucket_bytes = BYTES_PER_LAYER_HEURISTIC * (end - start) as u64;

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
    assignments.sort_by(|a, b| {
        a.node_pubkey
            .cmp(&b.node_pubkey)
            .then(a.model_id.0.cmp(&b.model_id.0))
    });

    if assignments.is_empty() {
        return None;
    }

    let input_snapshot_hash =
        compute_input_snapshot_hash(open_requests, capacity_ads, layer_counts);

    let _ = ad_by_pubkey;

    Some(ShardAssignmentProposalBody {
        epoch_blocks,
        assignments,
        input_snapshot_hash,
    })
}

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

        let total_ranges: usize = p.assignments.iter().map(|a| a.ranges.len()).sum();
        assert_eq!(total_ranges, 6);
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
        let m = b"m7b";
        let reqs = vec![req(m, b"r1", 1)];
        let ads = vec![ad(1, 1, "US")];
        let counts: LayerCounts = [(hash_bytes(m).0, 15u32)].into_iter().collect();
        let p = compute_assignment(&reqs, &ads, &counts, 1000).unwrap();
        let entry = p
            .assignments
            .iter()
            .find(|a| a.node_pubkey == [1u8; 32])
            .unwrap();
        assert!(entry.ranges.len() <= 2);
    }
}
