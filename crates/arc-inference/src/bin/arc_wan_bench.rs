//! `arc_wan_bench`: WAN-simulation benchmark for pipeline-sharded inference
//! (ENG-9).
//!
//! Runs a synthetic integer model split into 1–N stages connected by
//! persistent loopback TCP, with each hop shaped to a chosen RTT, jitter and
//! uplink, and reports:
//!
//! * per-answer and aggregate tok/s;
//! * a per-hop breakdown (serialize, transfer, queue, deserialize, compute);
//! * research-7's model prediction next to each measurement, and the per-hop
//!   overhead `o` the measurement implies;
//! * raw vs exact-bit-packed activations, overlap on/off, micro-batching;
//! * a placement-optimizer projection for a Kimi-K2.6-shaped model on a
//!   research-7 node population (labeled as a projection).
//!
//! Every networked run is checked bit for bit (tokens and every per-stage
//! commitment) against a local, transport-free reference; the binary exits
//! non-zero on any mismatch.
//!
//! ```text
//! arc_wan_bench --label "CI runner" --model small --out-json r.json --out-md r.md
//! arc_wan_bench --quick            # a few seconds, tiny model, for a local smoke check
//! ```

use arc_inference::cached_integer_model::CachedIntegerModel;
use arc_inference::stage_net::codec::CodecChoice;
use arc_inference::stage_net::cost::{self, HopCost};
use arc_inference::stage_net::pipeline::{
    Commitments, LinkKind, RingConfig, RingReport, Workload, even_splits, reference, run_ring,
};
use arc_inference::stage_net::placement::{self, ModelShape, NodeSpec, Objective, PlacementParams};
use arc_inference::stage_net::shaper::LinkProfile;
use arc_inference::stage_net::wire::{EncodeOptions, TcpTuning};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

#[derive(Clone, Debug)]
struct Args {
    label: String,
    model: String,
    stages: Vec<usize>,
    rtts: Vec<f64>,
    jitter_ms: f64,
    uplink_mbps: f64,
    wire_dim: usize,
    gen_tokens: usize,
    prompt: usize,
    in_flight: Vec<usize>,
    out_json: Option<String>,
    out_md: Option<String>,
    quick: bool,
}

fn parse_list<T: std::str::FromStr>(s: &str) -> Vec<T> {
    s.split(',').filter_map(|x| x.trim().parse().ok()).collect()
}

fn parse_args() -> Args {
    let mut a = Args {
        label: "unlabeled".into(),
        model: "small".into(),
        stages: vec![1, 2, 4],
        rtts: vec![0.0, 10.0, 30.0, 60.0],
        jitter_ms: 1.0,
        uplink_mbps: 50.0,
        wire_dim: 7168,
        gen_tokens: 12,
        prompt: 4,
        in_flight: vec![1, 4, 8, 16],
        out_json: None,
        out_md: None,
        quick: false,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let v = argv.get(i + 1).cloned().unwrap_or_default();
        match argv[i].as_str() {
            "--label" => a.label = v,
            "--model" => a.model = v,
            "--stages" => a.stages = parse_list(&v),
            "--rtt" => a.rtts = parse_list(&v),
            "--jitter" => a.jitter_ms = v.parse().unwrap_or(a.jitter_ms),
            "--uplink" => a.uplink_mbps = v.parse().unwrap_or(a.uplink_mbps),
            "--wire-dim" => a.wire_dim = v.parse().unwrap_or(a.wire_dim),
            "--gen" => a.gen_tokens = v.parse().unwrap_or(a.gen_tokens),
            "--prompt" => a.prompt = v.parse().unwrap_or(a.prompt),
            "--in-flight" => a.in_flight = parse_list(&v),
            "--out-json" => a.out_json = Some(v),
            "--out-md" => a.out_md = Some(v),
            "--quick" => {
                a.quick = true;
                i += 1;
                continue;
            }
            "-h" | "--help" => {
                println!(
                    "arc_wan_bench [--label L] [--model tiny|small|medium] [--stages 1,2,4] \
                     [--rtt 0,10,30,60] [--jitter MS] [--uplink MBPS] [--wire-dim N] [--gen N] \
                     [--prompt N] [--in-flight 1,4,8,16] [--out-json F] [--out-md F] [--quick]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
        i += 2;
    }
    if a.quick {
        a.model = "tiny".into();
        a.stages = vec![1, 2];
        a.rtts = vec![0.0, 10.0];
        a.gen_tokens = 4;
        a.in_flight = vec![1, 4];
    }
    a
}

/// (vocab, d_model, heads, d_ff, layers)
fn model_shape(name: &str) -> (usize, usize, usize, usize, usize) {
    match name {
        "tiny" => (256, 128, 4, 352, 8),
        "medium" => (4096, 2048, 16, 5632, 16),
        _ => (2048, 1024, 8, 2816, 16),
    }
}

// ─── Statistics ─────────────────────────────────────────────────────────────

fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let idx = ((s.len() - 1) as f64 * p).round() as usize;
    s[idx]
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        f64::NAN
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

#[derive(Serialize, Clone, Debug)]
struct HopSummary {
    hop: usize,
    messages: usize,
    wire_bytes_p50: f64,
    bits_per_value: Option<f64>,
    serialize_ms_p50: f64,
    transfer_ms_p50: f64,
    transfer_ms_p95: f64,
    queue_ms_p50: f64,
    deserialize_ms_p50: f64,
    /// Compute of the stage this hop delivers to (0 for the driver).
    next_compute_ms_p50: f64,
}

#[derive(Serialize, Clone, Debug)]
struct RunSummary {
    experiment: String,
    stages: usize,
    rtt_ms: f64,
    jitter_ms: f64,
    uplink_mbps: Option<f64>,
    wire_dim: usize,
    codec: String,
    overlap: bool,
    in_flight: usize,
    microbatch: usize,
    generated_tokens: usize,
    pass_ms_p50: f64,
    pass_ms_p95: f64,
    pass_ms_mean: f64,
    per_answer_tok_s: f64,
    aggregate_tok_s: f64,
    stage_compute_ms_p50: Vec<f64>,
    /// research-7 §2.1 with measured compute and codec cost, injected RTT and
    /// uplink, and o = 0.
    model_pass_ms: f64,
    /// (measured − model) / hops: the per-hop overhead the run implies.
    implied_o_ms: Option<f64>,
    bit_exact: bool,
    hops: Vec<HopSummary>,
}

struct Case {
    experiment: &'static str,
    stages: usize,
    rtt_ms: f64,
    uplink: Option<f64>,
    codec: CodecChoice,
    overlap: bool,
    in_flight: usize,
    microbatch: usize,
}

fn summarize(case: &Case, args: &Args, r: &RingReport, exact: bool, d_model: usize) -> RunSummary {
    let s = case.stages;
    let decode: Vec<f64> = r
        .passes
        .iter()
        .filter(|p| p.decode)
        .map(|p| ms(p.recv_ns - p.sent_ns))
        .collect();
    let first = r
        .passes
        .iter()
        .filter(|p| p.decode)
        .map(|p| p.sent_ns)
        .min()
        .unwrap_or(0);
    let last = r.passes.iter().map(|p| p.recv_ns).max().unwrap_or(0);
    let generated: usize = r.tokens.iter().map(Vec::len).sum();
    let window_s = (last.saturating_sub(first)) as f64 / 1e9;
    // Tokens produced inside the decode window: every generated token's pass
    // was sent at or after `first`.
    let aggregate = generated as f64 / window_s.max(1e-9);
    let mut by_stage: BTreeMap<usize, Vec<f64>> = BTreeMap::new();
    for c in &r.computes {
        by_stage
            .entry(c.stage)
            .or_default()
            .push(ms(c.compute_ns) / c.entries.max(1) as f64 * case.microbatch as f64);
    }
    let compute_p50: Vec<f64> = (0..s)
        .map(|i| by_stage.get(&i).map_or(0.0, |v| pct(v, 0.5)))
        .collect();
    let mut hops = Vec::new();
    let mut model_hops = Vec::new();
    if s > 1 {
        for h in 0..s {
            let samples: Vec<_> = r.hops.iter().filter(|x| x.hop == h).collect();
            let get =
                |f: &dyn Fn(&arc_inference::stage_net::pipeline::HopSample) -> f64| -> Vec<f64> {
                    samples.iter().map(|x| f(x)).collect()
                };
            let sends: Vec<_> = r.sends.iter().filter(|(hh, _)| *hh == h).collect();
            let values: usize = sends.iter().map(|(_, st)| st.values).sum();
            let payload: usize = sends.iter().map(|(_, st)| st.payload_bytes).sum();
            let wire = get(&|x| x.wire_bytes as f64);
            let ser = pct(&get(&|x| ms(x.serialize_ns)), 0.5);
            let de = pct(&get(&|x| ms(x.deserialize_ns)), 0.5);
            let q = pct(&get(&|x| ms(x.queue_ns)), 0.5);
            let summary = HopSummary {
                hop: h,
                messages: samples.len(),
                wire_bytes_p50: pct(&wire, 0.5),
                bits_per_value: (values > 0).then(|| payload as f64 * 8.0 / values as f64),
                serialize_ms_p50: ser,
                transfer_ms_p50: pct(&get(&|x| ms(x.transfer_ns)), 0.5),
                transfer_ms_p95: pct(&get(&|x| ms(x.transfer_ns)), 0.95),
                queue_ms_p50: q,
                deserialize_ms_p50: de,
                next_compute_ms_p50: pct(&get(&|x| ms(x.compute_ns)), 0.5),
            };
            model_hops.push(HopCost {
                rtt_ms: case.rtt_ms,
                overhead_ms: ser + de,
                uplink_mbps: case.uplink,
                bytes: summary.wire_bytes_p50,
            });
            hops.push(summary);
        }
    }
    let model_pass = cost::pass_ms(&compute_p50, &model_hops);
    let p50 = pct(&decode, 0.5);
    let _ = d_model;
    RunSummary {
        experiment: case.experiment.into(),
        stages: s,
        rtt_ms: case.rtt_ms,
        jitter_ms: if case.rtt_ms > 0.0 {
            args.jitter_ms
        } else {
            0.0
        },
        uplink_mbps: case.uplink,
        wire_dim: args.wire_dim,
        codec: match case.codec {
            CodecChoice::Raw => "raw i64".into(),
            CodecChoice::Auto => "exact bit-pack".into(),
        },
        overlap: case.overlap,
        in_flight: case.in_flight,
        microbatch: case.microbatch,
        generated_tokens: generated,
        pass_ms_p50: p50,
        pass_ms_p95: pct(&decode, 0.95),
        pass_ms_mean: mean(&decode),
        per_answer_tok_s: 1000.0 / mean(&decode),
        aggregate_tok_s: aggregate,
        stage_compute_ms_p50: compute_p50,
        model_pass_ms: model_pass,
        implied_o_ms: (s > 1 && case.in_flight == 1).then(|| (p50 - model_pass) / s as f64),
        bit_exact: exact,
        hops,
    }
}

/// Tokens and per-stage commitments from the transport-free reference.
type Reference = (Vec<Vec<u32>>, Commitments);

fn run_case(
    model: &Arc<CachedIntegerModel>,
    args: &Args,
    case: &Case,
    refs: &mut BTreeMap<(usize, usize), Reference>,
    golden_tokens: &mut BTreeMap<usize, Vec<Vec<u32>>>,
) -> Result<RunSummary, String> {
    let n_layers = model.config.n_layers;
    let splits = even_splits(n_layers, case.stages);
    let workload = Workload::synthetic(
        case.in_flight,
        args.prompt,
        args.gen_tokens,
        case.microbatch,
        model.config.vocab_size as u32,
    );
    let key = (case.stages, case.in_flight);
    if let std::collections::btree_map::Entry::Vacant(e) = refs.entry(key) {
        e.insert(reference(model, &splits, &workload)?);
    }
    let (ref_tokens, ref_commits) = &refs[&key];
    let shaped = case.rtt_ms > 0.0;
    let profiles = (0..case.stages)
        .map(|h| {
            if shaped {
                LinkProfile {
                    rtt_ms: case.rtt_ms,
                    jitter_ms: args.jitter_ms,
                    uplink_mbps: case.uplink,
                    seed: 0x5eed + h as u64,
                }
            } else {
                LinkProfile::ideal()
            }
        })
        .collect();
    let cfg = RingConfig {
        splits,
        link: LinkKind::Tcp(TcpTuning::default()),
        profiles,
        overlap: case.overlap,
        verify: true,
        encode: EncodeOptions {
            codec: case.codec,
            ballast_dim: (args.wire_dim > model.config.d_model).then_some(args.wire_dim),
        },
    };
    let r = run_ring(model.clone(), &cfg, &workload)?;
    let mut exact = r.tokens == *ref_tokens && r.commitments == *ref_commits;
    // Tokens must also be identical for every split.
    match golden_tokens.get(&case.in_flight) {
        Some(g) => exact &= *g == r.tokens,
        None => {
            golden_tokens.insert(case.in_flight, r.tokens.clone());
        }
    }
    Ok(summarize(case, args, &r, exact, model.config.d_model))
}

// ─── Placement projection ───────────────────────────────────────────────────

const GB: u64 = 1_000_000_000;

struct Projection {
    md: String,
    json: serde_json::Value,
}

/// A Kimi-K2.6-shaped model on a research-7 node population. Every input is
/// listed in the output with its source tag.
fn projection(bits_per_value: f64, o_ms: f64) -> Projection {
    // K2.6 shape (docs/protocol/kimi-k26-checkpoint.md on ENG-5's branch):
    // 61 layers, hidden 7,168, 384 routed experts, 582 GB with INT4 g32
    // experts and INT8 elsewhere, of which 571 GB routed experts; ~22.6 GB
    // read per token at batch 1 [CALC there].
    let emb = 163_840u64 * 7_168; // INT8 embedding; LM head the same.
    let total = 582 * GB;
    let layer_bytes = (total - 2 * emb) / 61;
    let read_per_layer_gb = 22.6 / 61.0;
    let routed_read_gb = 571.0 * 8.0 / 384.0; // active routed experts per token
    let routed_time_fraction = routed_read_gb / 22.6;
    let boundary = 7_168.0 * bits_per_value / 8.0;
    let model = ModelShape {
        n_layers: 61,
        layer_bytes,
        first_extra_bytes: emb,
        last_extra_bytes: emb,
        boundary_bytes: boundary,
        experts_per_layer: 384,
        routed_bytes_fraction: 571.0 / 582.0,
        routed_time_fraction,
    };
    // Per-layer decode time from memory bandwidth at an ASSUMED efficiency of
    // 0.6 of peak (llama.cpp-class; research-7 E1). ARC's integer engine does
    // not reach this today.
    let eff = 0.6;
    let ms_layer = |peak_gbs: f64| read_per_layer_gb / (peak_gbs * eff) * 1e3;
    // (id, metro, usable GB = 0.8 × installed per r6, peak GB/s, uplink Mb/s)
    let fleet: Vec<(&str, &str, u64, f64, f64)> = vec![
        ("nyc-lab-a", "NYC", 410, 819.0, 1000.0),
        ("nyc-lab-b", "NYC", 410, 819.0, 1000.0),
        ("nyc-1", "NYC", 205, 819.0, 50.0),
        ("nyc-2", "NYC", 102, 546.0, 50.0),
        ("dc-1", "DC", 205, 819.0, 50.0),
        ("dc-2", "DC", 154, 800.0, 50.0),
        ("chi-1", "CHI", 410, 819.0, 50.0),
        ("lon-1", "LON", 410, 819.0, 50.0),
        ("lon-2", "LON", 205, 819.0, 50.0),
        ("lon-3", "LON", 154, 800.0, 50.0),
        ("ams-1", "AMS", 205, 819.0, 50.0),
        ("ams-2", "AMS", 102, 546.0, 50.0),
        ("fra-1", "FRA", 410, 819.0, 50.0),
        ("fra-2", "FRA", 102, 546.0, 50.0),
    ];
    // DC-to-DC RTT between metros, ms (research-7 §1, MEASURED midpoints).
    // Pairs research-7 does not list are bounded through NYC or LON [CALC].
    let city = |a: &str, b: &str| -> f64 {
        let t: &[(&str, &str, f64)] = &[
            ("NYC", "DC", 8.0),
            ("NYC", "CHI", 17.5),
            ("LON", "AMS", 7.5),
            ("LON", "FRA", 15.0),
            ("AMS", "FRA", 8.0),
            ("NYC", "LON", 73.5),
        ];
        if a == b {
            return 0.0;
        }
        for &(x, y, v) in t {
            if (x == a && y == b) || (x == b && y == a) {
                return v;
            }
        }
        let us = |c: &str| matches!(c, "NYC" | "DC" | "CHI");
        let via = |c: &str, hub: &str| {
            if c == hub {
                0.0
            } else {
                city_direct(c, hub, t)
            }
        };
        if us(a) && us(b) {
            via(a, "NYC") + via(b, "NYC")
        } else if !us(a) && !us(b) {
            via(a, "LON") + via(b, "LON")
        } else {
            let (u, e) = if us(a) { (a, b) } else { (b, a) };
            via(u, "NYC") + 73.5 + via(e, "LON")
        }
    };
    // Home access leg: research-7 in-metro FTTH median 4.6 ms; the two lab
    // machines share one LAN (0.3 ms between them, Thunderbolt/10 GbE class).
    let access = |id: &str| if id.starts_with("nyc-lab") { 0.15 } else { 4.6 };
    let n = fleet.len();
    let nodes: Vec<NodeSpec> = fleet
        .iter()
        .map(|&(id, metro, mem, bw, up)| NodeSpec {
            id: id.into(),
            region: metro.into(),
            mem_bytes: mem * GB,
            ms_per_layer: ms_layer(bw),
            uplink_mbps: up,
        })
        .collect();
    let rtt_full: Vec<Vec<f64>> = (0..n)
        .map(|i| {
            (0..n)
                .map(|j| {
                    if i == j {
                        0.0
                    } else if fleet[i].0.starts_with("nyc-lab") && fleet[j].0.starts_with("nyc-lab")
                    {
                        0.3
                    } else {
                        city(fleet[i].1, fleet[j].1) + access(fleet[i].0) + access(fleet[j].0)
                    }
                })
                .collect()
        })
        .collect();
    let mut md = String::new();
    let mut json = Vec::new();
    let _ = writeln!(
        md,
        "Inputs: Kimi-K2.6 shape (61 layers, 7,168 hidden, 384 experts, 582 GB at INT4 experts; ENG-5 doc). \
         Boundary {boundary:.0} B/token/hop = 7,168 × {bits_per_value:.1} bits (exact codec, bits/value measured on the synthetic model in this run). \
         Per-layer decode {:.2} ms (819 GB/s) / {:.2} ms (546 GB/s) from {read_per_layer_gb:.3} GB read per layer per token at an ASSUMED 0.6 bandwidth efficiency. \
         Hop overhead o = {o_ms:.2} ms (measured in this run). RTTs: research-7 DC-to-DC medians plus 4.6 ms FTTH access per home. \
         **All rows below are projections, not measurements.** 59 tok/s per answer needs a pass ≤ 16.9 ms.\n",
        ms_layer(819.0),
        ms_layer(546.0)
    );
    let _ = writeln!(
        md,
        "| Scenario | Objective | Stages (nodes · layers · experts) | Compute ms | Network ms | Pass ms | Per-answer tok/s | Aggregate tok/s (64 in flight) | Cross-region |"
    );
    let _ = writeln!(md, "|---|---|---|---|---|---|---|---|---|");
    let scenarios: Vec<(&str, Vec<usize>, Objective)> = vec![
        (
            "All 14 nodes (NYC lab island present)",
            (0..n).collect(),
            Objective::PerAnswer,
        ),
        (
            "All 14 nodes",
            (0..n).collect(),
            Objective::Aggregate { in_flight: 64 },
        ),
        (
            "Without the NYC lab island (home WAN only)",
            (2..n).collect(),
            Objective::PerAnswer,
        ),
        (
            "Without the NYC lab island",
            (2..n).collect(),
            Objective::Aggregate { in_flight: 64 },
        ),
        (
            "Europe only, home WAN",
            (7..n).collect(),
            Objective::PerAnswer,
        ),
    ];
    for (name, keep, obj) in scenarios {
        let sub_nodes: Vec<NodeSpec> = keep.iter().map(|&i| nodes[i].clone()).collect();
        let sub_rtt: Vec<Vec<f64>> = keep
            .iter()
            .map(|&i| keep.iter().map(|&j| rtt_full[i][j]).collect())
            .collect();
        let params = PlacementParams {
            hop_overhead_ms: o_ms,
            cluster_diameter_ms: 60.0,
            objective: obj,
            ..PlacementParams::default()
        };
        let obj_s = match obj {
            Objective::PerAnswer => "per-answer".to_string(),
            Objective::Aggregate { in_flight } => format!("aggregate ({in_flight})"),
        };
        match placement::plan(&sub_nodes, &sub_rtt, &model, &params) {
            Ok(p) => {
                let compute: Vec<f64> = p.stages.iter().map(|s| s.compute_ms).collect();
                let hops: Vec<HopCost> = p.stages.iter().filter_map(|s| s.hop).collect();
                let agg64 = cost::aggregate_tok_s(&compute, &hops, 64);
                let stages: Vec<String> = p
                    .stages
                    .iter()
                    .map(|s| {
                        let members: Vec<String> = s
                            .members
                            .iter()
                            .map(|m| {
                                if s.members.len() > 1 {
                                    format!(
                                        "{}[e{}..{}]",
                                        sub_nodes[m.node].id, m.experts.start, m.experts.end
                                    )
                                } else {
                                    sub_nodes[m.node].id.clone()
                                }
                            })
                            .collect();
                        format!(
                            "{} · L{}..{}",
                            members.join("+"),
                            s.layers.start,
                            s.layers.end
                        )
                    })
                    .collect();
                let _ = writeln!(
                    md,
                    "| {name} | {obj_s} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {} |",
                    stages.join(" → "),
                    p.compute_ms,
                    p.network_ms,
                    p.pass_ms,
                    p.per_answer_tok_s,
                    agg64,
                    if p.cross_cluster { "yes" } else { "no" }
                );
                json.push(serde_json::json!({
                    "scenario": name, "objective": obj_s, "stages": stages,
                    "compute_ms": p.compute_ms, "network_ms": p.network_ms, "pass_ms": p.pass_ms,
                    "per_answer_tok_s": p.per_answer_tok_s, "aggregate_tok_s_64": agg64,
                    "cross_cluster": p.cross_cluster, "label": "PROJECTION"
                }));
            }
            Err(e) => {
                let _ = writeln!(md, "| {name} | {obj_s} | infeasible: {e} | | | | | | |");
            }
        }
    }
    Projection {
        md,
        json: serde_json::Value::Array(json),
    }
}

fn city_direct(a: &str, b: &str, t: &[(&str, &str, f64)]) -> f64 {
    t.iter()
        .find(|&&(x, y, _)| (x == a && y == b) || (x == b && y == a))
        .map_or(f64::INFINITY, |&(_, _, v)| v)
}

// ─── Main ───────────────────────────────────────────────────────────────────

fn fmt_opt(v: Option<f64>, digits: usize) -> String {
    v.map_or("—".into(), |x| format!("{x:.digits$}"))
}

fn main() {
    let args = parse_args();
    let (vs, d, nh, dff, nl) = model_shape(&args.model);
    let model = Arc::new(CachedIntegerModel::synthetic(7, vs, d, nh, dff, nl));
    let (timer_mode, sleep_1ms) = arc_inference::stage_net::shaper::timer_calibration();
    let host = format!(
        "{} {} ({} threads; WAN timer mode {:?}: a 1 ms sleep took {:.2} ms)",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map_or(0, |n| n.get()),
        timer_mode,
        sleep_1ms.as_secs_f64() * 1e3
    );
    eprintln!(
        "arc_wan_bench: label={} model={} (d={d}, layers={nl}) host={host}",
        args.label, args.model
    );
    let mut cases: Vec<Case> = Vec::new();
    // A. Per-answer speed and per-hop breakdown.
    for &s in &args.stages {
        for &r in &args.rtts {
            if s == 1 && r > 0.0 {
                continue;
            }
            cases.push(Case {
                experiment: "A per-answer",
                stages: s,
                rtt_ms: r,
                uplink: (r > 0.0).then_some(args.uplink_mbps),
                codec: CodecChoice::Auto,
                overlap: true,
                in_flight: 1,
                microbatch: 1,
            });
        }
    }
    let mid_rtt = if args.rtts.contains(&30.0) {
        30.0
    } else {
        *args.rtts.last().unwrap_or(&10.0)
    };
    let max_s = *args.stages.iter().max().unwrap_or(&2);
    // B. Codec: raw vs exact bit-pack on a bounded uplink.
    if max_s >= 2 {
        for codec in [CodecChoice::Raw, CodecChoice::Auto] {
            cases.push(Case {
                experiment: "B codec",
                stages: 2,
                rtt_ms: mid_rtt,
                uplink: Some(args.uplink_mbps),
                codec,
                overlap: true,
                in_flight: 1,
                microbatch: 1,
            });
        }
    }
    // C. Swarm throughput: in-flight sequences × overlap, then micro-batching.
    for &m in &args.in_flight {
        for overlap in [false, true] {
            cases.push(Case {
                experiment: "C swarm",
                stages: max_s,
                rtt_ms: mid_rtt,
                uplink: Some(args.uplink_mbps),
                codec: CodecChoice::Auto,
                overlap,
                in_flight: m,
                microbatch: 1,
            });
        }
    }
    let big = *args.in_flight.iter().max().unwrap_or(&1);
    if big >= 4 {
        cases.push(Case {
            experiment: "C swarm",
            stages: max_s,
            rtt_ms: mid_rtt,
            uplink: Some(args.uplink_mbps),
            codec: CodecChoice::Auto,
            overlap: true,
            in_flight: big,
            microbatch: 4,
        });
    }

    let mut refs = BTreeMap::new();
    let mut golden = BTreeMap::new();
    let mut results = Vec::new();
    let mut failed = false;
    for c in &cases {
        match run_case(&model, &args, c, &mut refs, &mut golden) {
            Ok(r) => {
                eprintln!(
                    "  {:<13} S={} rtt={:>4} uplink={:>5} {:<14} overlap={:<5} inflight={:>2} mb={} → pass p50 {:>7.2} ms, {:>7.1} tok/s/answer, {:>7.1} tok/s aggregate, exact={}",
                    r.experiment,
                    r.stages,
                    r.rtt_ms,
                    fmt_opt(r.uplink_mbps, 0),
                    r.codec,
                    r.overlap,
                    r.in_flight,
                    r.microbatch,
                    r.pass_ms_p50,
                    r.per_answer_tok_s,
                    r.aggregate_tok_s,
                    r.bit_exact
                );
                failed |= !r.bit_exact;
                results.push(r);
            }
            Err(e) => {
                eprintln!("  run failed: {e}");
                failed = true;
            }
        }
    }

    // Loopback-implied overhead o (median over in-flight-1 multi-stage runs
    // with no shaping), used for the projection.
    let o_vals: Vec<f64> = results
        .iter()
        .filter(|r| r.rtt_ms == 0.0 && r.stages > 1)
        .filter_map(|r| r.implied_o_ms)
        .collect();
    let o_ms = if o_vals.is_empty() {
        1.0
    } else {
        pct(&o_vals, 0.5).max(0.0)
    };
    let bits: Vec<f64> = results
        .iter()
        .filter(|r| r.codec == "exact bit-pack")
        .flat_map(|r| r.hops.iter().filter_map(|h| h.bits_per_value))
        .collect();
    let bits_pv = if bits.is_empty() {
        64.0
    } else {
        pct(&bits, 0.5)
    };
    let proj = projection(bits_pv, o_ms);

    let md = render_md(
        &args,
        &host,
        (vs, d, nh, dff, nl),
        &results,
        &proj,
        o_ms,
        bits_pv,
    );
    print!("{md}");
    if let Some(p) = &args.out_md {
        std::fs::write(p, &md).expect("write markdown");
    }
    if let Some(p) = &args.out_json {
        let v = serde_json::json!({
            "label": args.label,
            "host": host,
            "model": {"name": args.model, "vocab": vs, "d_model": d, "heads": nh, "d_ff": dff, "layers": nl},
            "jitter_ms": args.jitter_ms,
            "uplink_mbps": args.uplink_mbps,
            "wire_dim": args.wire_dim,
            "implied_o_ms_loopback": o_ms,
            "bits_per_value_p50": bits_pv,
            "runs": results,
            "projection": proj.json,
        });
        std::fs::write(p, serde_json::to_string_pretty(&v).expect("json")).expect("write json");
    }
    if failed {
        eprintln!("FAIL: a run failed or was not bit-exact");
        std::process::exit(1);
    }
}

fn render_md(
    args: &Args,
    host: &str,
    shape: (usize, usize, usize, usize, usize),
    results: &[RunSummary],
    proj: &Projection,
    o_ms: f64,
    bits_pv: f64,
) -> String {
    let (vs, d, nh, dff, nl) = shape;
    let mut md = String::new();
    let _ = writeln!(md, "## arc_wan_bench — {} (measured)\n", args.label);
    let _ = writeln!(
        md,
        "Host: {host}. Synthetic integer model `{}`: vocab {vs}, d_model {d}, {nh} heads, d_ff {dff}, {nl} layers. \
         Stages are threads joined by persistent loopback TCP (nodelay, 4 MiB buffers); each hop is shaped to the RTT shown \
         (one-way = RTT/2, jitter ±{} ms, uplink {} Mb/s). Hidden states are tiled to {} values on the wire so every hop \
         carries Kimi-width frames. Prompt {} tokens, {} generated per sequence. \
         \"Exact\" = tokens and every per-stage commitment equal a local transport-free reference, and tokens equal across splits.\n",
        args.model, args.jitter_ms, args.uplink_mbps, args.wire_dim, args.prompt, args.gen_tokens
    );
    let _ = writeln!(
        md,
        "Simulator accounting only: the prediction reuses same-run compute/codec medians and the injected RTT/uplink formula. Agreement does not validate real-WAN Kimi performance. Rates use mean latency; pass columns report quantiles.\n"
    );
    let _ = writeln!(md, "### A. Per-answer speed (1 sequence in flight)\n");
    let _ = writeln!(
        md,
        "| Stages | Hop RTT ms | Uplink Mb/s | Pass p50 ms | Pass p95 ms | tok/s per answer | research-7 model ms (o = 0) | Implied o per hop ms | Exact |"
    );
    let _ = writeln!(md, "|---|---|---|---|---|---|---|---|---|");
    for r in results.iter().filter(|r| r.experiment == "A per-answer") {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {:.2} | {:.2} | {:.1} | {:.2} | {} | {} |",
            r.stages,
            if r.rtt_ms > 0.0 {
                format!("{}", r.rtt_ms)
            } else {
                "0 (loopback)".into()
            },
            fmt_opt(r.uplink_mbps, 0),
            r.pass_ms_p50,
            r.pass_ms_p95,
            r.per_answer_tok_s,
            r.model_pass_ms,
            fmt_opt(r.implied_o_ms, 2),
            if r.bit_exact { "yes" } else { "**NO**" }
        );
    }
    let _ = writeln!(md, "\n### Per-hop breakdown (p50, ms; experiment A)\n");
    let _ = writeln!(
        md,
        "| Stages | Hop RTT | Hop | Bytes/msg | Bits/value | Serialize | Transfer p50 | Transfer p95 | Queue | Deserialize | Next stage compute |"
    );
    let _ = writeln!(md, "|---|---|---|---|---|---|---|---|---|---|---|");
    for r in results
        .iter()
        .filter(|r| r.experiment == "A per-answer" && r.stages > 1)
    {
        for h in &r.hops {
            let to = if h.hop + 1 == r.stages {
                "driver".to_string()
            } else {
                format!("{}", h.hop + 1)
            };
            let _ = writeln!(
                md,
                "| {} | {} | {}→{} | {:.0} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} |",
                r.stages,
                r.rtt_ms,
                h.hop,
                to,
                h.wire_bytes_p50,
                fmt_opt(h.bits_per_value, 1),
                h.serialize_ms_p50,
                h.transfer_ms_p50,
                h.transfer_ms_p95,
                h.queue_ms_p50,
                h.deserialize_ms_p50,
                h.next_compute_ms_p50
            );
        }
    }
    let _ = writeln!(md, "\n### B. Exact activation compression (2 stages)\n");
    let _ = writeln!(
        md,
        "| Codec | Hop RTT | Uplink | Bytes/msg (hop 0) | Bits/value | Transfer p50 hop 0 ms | Pass p50 ms | tok/s per answer | Exact |"
    );
    let _ = writeln!(md, "|---|---|---|---|---|---|---|---|---|");
    for r in results.iter().filter(|r| r.experiment == "B codec") {
        let h = r.hops.first();
        let _ = writeln!(
            md,
            "| {} | {} | {} | {:.0} | {} | {:.3} | {:.2} | {:.1} | {} |",
            r.codec,
            r.rtt_ms,
            fmt_opt(r.uplink_mbps, 0),
            h.map_or(f64::NAN, |h| h.wire_bytes_p50),
            fmt_opt(h.and_then(|h| h.bits_per_value), 1),
            h.map_or(f64::NAN, |h| h.transfer_ms_p50),
            r.pass_ms_p50,
            r.per_answer_tok_s,
            if r.bit_exact { "yes" } else { "**NO**" }
        );
    }
    let _ = writeln!(
        md,
        "\n### C. Swarm throughput: sequences in flight, overlap, micro-batching\n"
    );
    let _ = writeln!(
        md,
        "| Stages | Hop RTT | In flight | Micro-batch | Overlap | Aggregate tok/s | Per-answer tok/s | Pass p50 ms | Exact |"
    );
    let _ = writeln!(md, "|---|---|---|---|---|---|---|---|---|");
    for r in results.iter().filter(|r| r.experiment == "C swarm") {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} | {:.1} | {:.1} | {:.2} | {} |",
            r.stages,
            r.rtt_ms,
            r.in_flight,
            r.microbatch,
            if r.overlap { "on" } else { "off" },
            r.aggregate_tok_s,
            r.per_answer_tok_s,
            r.pass_ms_p50,
            if r.bit_exact { "yes" } else { "**NO**" }
        );
    }
    let _ = writeln!(
        md,
        "\nLoopback-implied per-hop overhead o = {o_ms:.3} ms; exact codec median {bits_pv:.1} bits/value on this model's activations.\n"
    );
    let _ = writeln!(
        md,
        "### D. Placement optimizer on a Kimi-K2.6-shaped model (PROJECTION)\n"
    );
    md.push_str(&proj.md);
    md
}
