//! `arc-island`: one MLA + MoE model (the Kimi K2 text architecture) split
//! across several processes or machines, bit-exact, with per-stage hash
//! commitments. See `crates/arc-inference/src/modern/mla/island/mod.rs`.
//!
//! This is an additional engine path. It does not touch consensus, rewards,
//! native inference, nodes or keys; every process it starts listens on the
//! addresses it is given (the benchmark uses 127.0.0.1 only).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use arc_inference::canonical_simd;
use arc_inference::modern::ModernError;
use arc_inference::modern::arith::Selection;
use arc_inference::modern::hex_lower;
use arc_inference::modern::mla::config::{ExpertFormat, MlaConfig};
use arc_inference::modern::mla::island::coordinator::{Completion, Coordinator, Request, Schedule};
use arc_inference::modern::mla::island::even_cuts;
use arc_inference::modern::mla::island::expert::{ExpertPlacement, RemoteExperts, serve_experts};
use arc_inference::modern::mla::island::process::ProcessIsland;
use arc_inference::modern::mla::island::transport::{
    ShapedTransport, TcpTransport, Transport, WanProfile, timer_mode,
};
use arc_inference::modern::mla::island::wire::activation_width;
use arc_inference::modern::mla::island::worker::{Fault, StageWorker, WorkerConfig, serve};
use arc_inference::modern::mla::model::{ExpertPool, MlaGeneration, StageModel};
use arc_inference::modern::mla::package::{self, StageSpec};
use arc_inference::modern::mla::synthetic::{self, Router};
use arc_inference::modern::model::GenerationRequest;
use serde_json::{Value, json};

const USAGE: &str = "usage: arc-island <command> [options]

  synth      --shape tiny|tiny-lora|tiny-i4|small|kimi-mini --out PKG [--layers A:B]
             [--router random|paired|flat]
  stage      --package PKG [--layers A:B] --listen ADDR --next ADDR [--log-dir DIR]
             [--fault tamper:SEQ:POS|lie:SEQ:POS]
             [--wan-ms MS --wan-jitter-ms MS --wan-mbit MBIT --wan-seed N]
             [--experts-at ADDR0,ADDR1,... --expert-device K]
             [--kernel scalar|simd] [--threads N]
  experts    --package PKG [--layers A:B] --listen ADDR [--kernel scalar|simd] [--threads N]
  reference  --package PKG --requests REQ.json --out OUT.json
  run        --package PKG --first ADDR --listen ADDR --requests REQ.json --out OUT.json
             [--micro-batches G] [--concurrency B] [--prefill-chunk N] [--shutdown]
  bench      --package PKG --out BENCH.json [--label TEXT] [--exe PATH]
             [--stages 1,2,4] [--pings N] [--payloads B,B,...]
             [--requests N] [--prompt-len N] [--max-tokens N]
             [--wan-stages 2,4,8] [--wan-ms 10,30,60] [--wan-mbit 20,100]
             [--wan-depths 1,4,16] [--wan-jitter-frac F] [--wan-wire-bytes B]
             [--wan-max-tokens N] [--wan-prompt-len N] [--skip-wan]
             [--kernel scalar|simd] [--stage-threads N]

A stage process prints one JSON line {\"listening\": ADDR, ...} when it is ready
and one line of statistics when it shuts down. Requests files hold
{\"requests\": [{\"id\", \"prompt\", \"max_tokens\", \"eos\", \"selection\"}]}.";

struct Args {
    items: Vec<String>,
}

impl Args {
    fn value(&self, name: &str) -> Option<String> {
        self.items
            .iter()
            .position(|a| a == name)
            .and_then(|i| self.items.get(i + 1).cloned())
    }

    fn required(&self, name: &str) -> Result<String, ModernError> {
        self.value(name)
            .ok_or_else(|| ModernError::Invalid(format!("missing {name}\n\n{USAGE}")))
    }

    fn path(&self, name: &str) -> Result<PathBuf, ModernError> {
        self.required(name).map(PathBuf::from)
    }

    fn flag(&self, name: &str) -> bool {
        self.items.iter().any(|a| a == name)
    }

    fn number(&self, name: &str, default: usize) -> Result<usize, ModernError> {
        self.parsed(name, default)
    }

    fn float(&self, name: &str, default: f64) -> Result<f64, ModernError> {
        self.parsed(name, default)
    }

    fn parsed<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T, ModernError> {
        match self.value(name) {
            None => Ok(default),
            Some(text) => text
                .parse()
                .map_err(|_| ModernError::Invalid(format!("{name}: {text:?} is not a number"))),
        }
    }

    fn list<T: std::str::FromStr>(&self, name: &str, default: &str) -> Result<Vec<T>, ModernError> {
        self.value(name)
            .unwrap_or_else(|| default.to_string())
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.trim()
                    .parse()
                    .map_err(|_| ModernError::Invalid(format!("{name}: {s:?} is not a number")))
            })
            .collect()
    }
}

fn write_json(path: &Path, value: &Value) -> Result<(), ModernError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| ModernError::Invalid(format!("JSON: {e}")))?;
    std::fs::write(path, text + "\n")
        .map_err(|e| ModernError::Io(format!("{}: {e}", path.display())))
}

fn read_json(path: &Path) -> Result<Value, ModernError> {
    let bytes =
        std::fs::read(path).map_err(|e| ModernError::Io(format!("{}: {e}", path.display())))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| ModernError::Invalid(format!("{}: {e}", path.display())))
}

fn configure(args: &Args) -> Result<(), ModernError> {
    let threads = args.number("--threads", 0)?;
    if threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()
            .map_err(|e| ModernError::Invalid(format!("thread pool: {e}")))?;
    }
    let _ = canonical_simd::fast_canonical_kernel_enabled();
    match args.value("--kernel").as_deref().unwrap_or("scalar") {
        "scalar" => canonical_simd::set_fast_canonical_kernel(false),
        "simd" => canonical_simd::set_fast_canonical_kernel(true),
        other => return Err(ModernError::Invalid(format!("unknown kernel {other}"))),
    }
    Ok(())
}

fn platform() -> Value {
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "logical_cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "rayon_threads": rayon::current_num_threads(),
        "wan_timer_mode": timer_mode(),
    })
}

fn hex(h: &[u8; 32]) -> String {
    hex_lower(h)
}

fn say(value: &Value) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

fn layers(args: &Args) -> Result<Option<StageSpec>, ModernError> {
    args.value("--layers")
        .map(|t| StageSpec::parse(&t))
        .transpose()
}

// ------------------------------------------------------------- commands --

fn cmd_synth(args: &Args) -> Result<(), ModernError> {
    let c = synthetic::shape(&args.required("--shape")?)?;
    let stage = layers(args)?.unwrap_or_else(|| StageSpec::full(&c));
    let router = Router::parse(args.value("--router").as_deref().unwrap_or("random"))?;
    let out = args.path("--out")?;
    synthetic::write_package(&c, stage, router, &out)?;
    let digest = package::digest_file(&out)?;
    say(
        &json!({"package": out.display().to_string(), "digest": digest.to_json(), "config": c.to_json()}),
    );
    Ok(())
}

fn cmd_stage(args: &Args) -> Result<(), ModernError> {
    configure(args)?;
    let mut model = StageModel::open_range(&args.path("--package")?, layers(args)?)?;
    if let Some(list) = args.value("--experts-at") {
        let devices: Vec<String> = list.split(',').map(str::to_string).collect();
        let placement = ExpertPlacement {
            devices: devices.len(),
            local: args.number("--expert-device", 0)?,
        };
        let pool = RemoteExperts::connect(&TcpTransport, placement, &devices)?;
        model.set_expert_pool(Some(Arc::new(pool) as Arc<dyn ExpertPool>));
    }
    let config = WorkerConfig {
        log_dir: args.value("--log-dir").map(PathBuf::from),
        fault: args
            .value("--fault")
            .map(|f| Fault::parse(&f))
            .transpose()?,
    };
    let worker = StageWorker::new(model, config)?;
    let tcp: Arc<dyn Transport> = Arc::new(TcpTransport);
    let transport: Arc<dyn Transport> = match args.value("--wan-ms") {
        Some(_) => Arc::new(ShapedTransport {
            inner: tcp.clone(),
            profile: WanProfile {
                one_way_ms: args.float("--wan-ms", 0.0)?,
                jitter_ms: args.float("--wan-jitter-ms", 0.0)?,
                uplink_mbit: args.float("--wan-mbit", 0.0)?,
                seed: args.number("--wan-seed", 1)? as u64,
            },
        }),
        None => tcp.clone(),
    };
    let listener = tcp
        .listen(&args.required("--listen")?)
        .map_err(|e| ModernError::Io(format!("listen: {e}")))?;
    let stage = worker.model().stage();
    say(&json!({
        "listening": listener.address(),
        "stage": [stage.first_layer, stage.end_layer],
        "replayed_positions": worker.stats.replayed_positions,
        "pid": std::process::id(),
        "wan_timer_mode": args.value("--wan-ms").map(|_| timer_mode()),
    }));
    let worker = serve(worker, listener, transport, args.required("--next")?)?;
    let s = &worker.stats;
    say(&json!({
        "stage": [stage.first_layer, stage.end_layer],
        "frames": s.frames,
        "items": s.items,
        "positions": s.positions,
        "compute_seconds": s.compute_seconds,
        "replayed_positions": s.replayed_positions,
    }));
    Ok(())
}

fn cmd_experts(args: &Args) -> Result<(), ModernError> {
    configure(args)?;
    let model = StageModel::open_range(&args.path("--package")?, layers(args)?)?;
    let listener = TcpTransport
        .listen(&args.required("--listen")?)
        .map_err(|e| ModernError::Io(format!("listen: {e}")))?;
    let stage = model.stage();
    say(
        &json!({"listening": listener.address(), "experts_for": [stage.first_layer, stage.end_layer]}),
    );
    serve_experts(Arc::new(model), listener);
    Ok(())
}

fn requests_from(value: &Value) -> Result<Vec<Request>, ModernError> {
    let ids = |v: &Value| -> Result<Vec<u32>, ModernError> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_u64().map(|x| x as u32))
                    .collect()
            })
            .ok_or_else(|| ModernError::Invalid("expected a list of token ids".into()))
    };
    value["requests"]
        .as_array()
        .ok_or_else(|| ModernError::Invalid("requests file without \"requests\"".into()))?
        .iter()
        .map(|r| {
            Ok(Request {
                id: r["id"]
                    .as_u64()
                    .ok_or_else(|| ModernError::Invalid("request id".into()))?,
                prompt: ids(&r["prompt"])?,
                max_tokens: r["max_tokens"].as_u64().unwrap_or(1) as usize,
                eos: if r["eos"].is_null() {
                    Vec::new()
                } else {
                    ids(&r["eos"])?
                },
                selection: Selection::parse(r["selection"].as_str().unwrap_or("rp64-argmax"))?,
            })
        })
        .collect()
}

fn generation_json(id: u64, g: &MlaGeneration) -> Value {
    json!({
        "id": id,
        "tokens": g.tokens,
        "output_hash": hex(&g.output_hash),
        "logits_digest": hex(&g.logits_digest),
        "boundary_digests": g.boundary_digests.iter().map(hex).collect::<Vec<_>>(),
    })
}

fn completion_json(c: &Completion) -> Value {
    json!({
        "id": c.id,
        "tokens": c.tokens,
        "output_hash": hex(&c.output_hash()),
        "logits_digest": hex(&c.logits_digest()),
        "boundary_digests": c.ledger.boundary_digests().map(|d| d.iter().map(hex).collect::<Vec<_>>()),
        "stage_roots": c.ledger.stage_ranges().iter().map(|&(a, b)| json!({
            "layers": [a, b],
            "root": c.ledger.stage_root(c.id, a, b).map(|r| hex(&r)),
        })).collect::<Vec<_>>(),
        "link_faults": c.ledger.link_faults.len(),
        "error": c.error,
    })
}

fn reference_generations(
    model: &StageModel,
    requests: &[Request],
) -> Result<Vec<MlaGeneration>, ModernError> {
    requests
        .iter()
        .map(|r| {
            model.generate(&GenerationRequest {
                prompt: &r.prompt,
                max_tokens: r.max_tokens,
                eos: &r.eos,
                selection: r.selection,
            })
        })
        .collect()
}

fn cmd_reference(args: &Args) -> Result<(), ModernError> {
    configure(args)?;
    let model = StageModel::open(&args.path("--package")?)?;
    let requests = requests_from(&read_json(&args.path("--requests")?)?)?;
    let generations = reference_generations(&model, &requests)?;
    let results: Vec<Value> = requests
        .iter()
        .zip(&generations)
        .map(|(r, g)| generation_json(r.id, g))
        .collect();
    write_json(&args.path("--out")?, &json!({"results": results}))
}

fn cmd_run(args: &Args) -> Result<(), ModernError> {
    let config = package::read_header_file(&args.path("--package")?)?.config;
    let transport: Arc<dyn Transport> = Arc::new(TcpTransport);
    let listener = transport
        .listen(&args.required("--listen")?)
        .map_err(|e| ModernError::Io(format!("listen: {e}")))?;
    let mut coordinator = Coordinator::new(transport, args.required("--first")?, listener, config);
    let requests = requests_from(&read_json(&args.path("--requests")?)?)?;
    let schedule = Schedule {
        micro_batches: args.number("--micro-batches", 1)?,
        concurrency: args.number("--concurrency", 1)?,
        prefill_chunk: args.number("--prefill-chunk", 0)?,
        ..Schedule::default()
    };
    let (done, stats) = coordinator.run(&requests, &schedule)?;
    if args.flag("--shutdown") {
        coordinator.shutdown()?;
    }
    write_json(
        &args.path("--out")?,
        &json!({
            "results": done.iter().map(completion_json).collect::<Vec<_>>(),
            "seconds": stats.seconds,
            "generated_tokens": stats.generated_tokens,
        }),
    )
}

// ---------------------------------------------------------------- bench --

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[i]
}

fn bench_requests(
    c: &MlaConfig,
    count: usize,
    prompt_len: usize,
    max_tokens: usize,
) -> Vec<Request> {
    let mut s = 0x5eed_u64;
    let mut next = || {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        s >> 33
    };
    (0..count)
        .map(|i| Request {
            id: i as u64 + 1,
            prompt: (0..prompt_len)
                .map(|_| (next() % c.vocab_size as u64) as u32)
                .collect(),
            max_tokens,
            eos: Vec::new(),
            selection: Selection::Rp64Argmax,
        })
        .collect()
}

fn matches_reference(done: &[Completion], reference: &[MlaGeneration]) -> bool {
    done.len() == reference.len()
        && done.iter().zip(reference).all(|(c, g)| {
            c.error.is_none()
                && c.tokens == g.tokens
                && c.logits_hashes == g.logits_hashes
                && c.ledger.boundary_digests().as_ref() == Some(&g.boundary_digests)
        })
}

fn answer_rates(done: &[Completion]) -> (f64, f64) {
    let rates: Vec<f64> = done
        .iter()
        .filter_map(Completion::decode_tokens_per_second)
        .collect();
    if rates.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    let mean = rates.iter().sum::<f64>() / rates.len() as f64;
    let mut sorted = rates;
    sorted.sort_by(f64::total_cmp);
    (mean, percentile(&sorted, 0.5))
}

/// Active INT weight bytes one token reads (attention, dense or the chosen
/// experts plus shared experts, LM head; the embedding is one row).
fn active_weight_bytes(c: &MlaConfig) -> f64 {
    let expert_bytes_per_param = match c.expert_format {
        ExpertFormat::Int8Dyadic => 1.0,
        ExpertFormat::Int4G32 => 0.5 + 2.0 / 32.0,
    };
    let d = c.d_model as f64;
    let q = if c.q_lora_rank > 0 {
        d * c.q_lora_rank as f64 + c.q_lora_rank as f64 * c.d_q() as f64
    } else {
        d * c.d_q() as f64
    };
    let attention = q
        + d * c.d_kv_a() as f64
        + (c.n_heads * c.qk_nope_dim * c.kv_lora_rank) as f64
        + (c.n_heads * c.v_head_dim * c.kv_lora_rank) as f64
        + d * c.d_attn_out() as f64;
    let dense_ffn = 3.0 * d * c.d_ff as f64;
    let expert = 3.0 * d * c.moe_d_ff as f64;
    let moe = c.n_experts_per_tok as f64 * expert * expert_bytes_per_param
        + 3.0 * d * c.shared_d_ff() as f64
        + d * c.n_routed_experts as f64 * 2.0;
    let mut total = d * c.vocab_size as f64; // LM head
    for l in 0..c.n_layers {
        total += attention + if c.is_moe(l) { moe } else { dense_ffn };
    }
    total
}

/// Lossless wire width of every boundary vector the reference requests
/// produce (bytes per value: 1, 2, 4 or 8).
fn boundary_widths(
    model: &StageModel,
    requests: &[Request],
    gens: &[MlaGeneration],
) -> Result<Value, ModernError> {
    let c = model.config();
    let mut histogram = [0u64; 4];
    let mut bytes = 0u64;
    let mut vectors = 0u64;
    let path =
        std::env::temp_dir().join(format!("arc-island-widths-{}.arcspkg", std::process::id()));
    std::fs::write(&path, model.bytes()).map_err(|e| ModernError::Io(format!("{e}")))?;
    for l in 1..c.n_layers {
        let prefix = StageModel::open_range(
            &path,
            Some(StageSpec {
                first_layer: 0,
                end_layer: l,
            }),
        )?;
        for (r, g) in requests.iter().zip(gens) {
            let mut tokens = r.prompt.clone();
            tokens.extend_from_slice(&g.tokens[..g.tokens.len() - 1]);
            let run = prefix.run_sequence(&tokens, None, r.prompt.len(), r.selection)?;
            for row in run.hidden.chunks_exact(c.d_model) {
                let w = activation_width(row);
                histogram[w.trailing_zeros() as usize] += 1;
                bytes += (w * c.d_model) as u64;
                vectors += 1;
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(json!({
        "vectors": vectors,
        "width_histogram": {"i8": histogram[0], "i16": histogram[1], "i32": histogram[2], "i64": histogram[3]},
        "mean_bytes_per_vector": bytes as f64 / vectors.max(1) as f64,
        "i64_bytes_per_vector": c.d_model * 8,
    }))
}

fn cmd_bench(args: &Args) -> Result<(), ModernError> {
    configure(args)?;
    let exe = match args.value("--exe") {
        Some(p) => PathBuf::from(p),
        None => std::env::current_exe().map_err(|e| ModernError::Io(format!("{e}")))?,
    };
    let package_path = args.path("--package")?;
    let model = StageModel::open(&package_path)?;
    let c = model.config().clone();
    let label = args.value("--label").unwrap_or_else(|| "unlabelled".into());
    let digest = package::digest_file(&package_path)?;
    let count = args.number("--requests", 8)?;
    let prompt_len = args.number("--prompt-len", 4)?;
    let max_tokens = args.number("--max-tokens", 16)?;
    let requests = bench_requests(&c, count, prompt_len, max_tokens);
    let started = Instant::now();
    let reference = reference_generations(&model, &requests)?;
    let single_seconds = started.elapsed().as_secs_f64();
    let positions: usize = reference
        .iter()
        .zip(&requests)
        .map(|(g, r)| r.prompt.len() + g.tokens.len() - 1)
        .sum();
    let decode: (f64, usize) = reference.iter().fold((0.0, 0), |(s, n), g| {
        (s + g.decode_seconds, n + g.decode_forwards)
    });
    let compute = json!({
        "single_process_seconds": single_seconds,
        "single_process_positions": positions,
        "single_process_ms_per_position": single_seconds * 1e3 / positions as f64,
        "single_process_decode_tok_s": decode.1 as f64 / decode.0,
        "active_weight_bytes_per_token": active_weight_bytes(&c),
        "effective_weight_gb_s": active_weight_bytes(&c) * decode.1 as f64 / decode.0 / 1e9,
    });
    eprintln!("compute: {compute}");
    let widths = boundary_widths(
        &model,
        &requests[..requests.len().min(4)],
        &reference[..requests.len().min(4)],
    )?;
    let mut failures = Vec::new();
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let kernel = args.value("--kernel").unwrap_or_else(|| "scalar".into());
    let stage_threads = args.number("--stage-threads", 0)?;
    // Each stage process gets the kernel and an even share of the cores.
    let stage_args = move |stages: usize| -> Vec<String> {
        let threads = if stage_threads > 0 {
            stage_threads
        } else {
            (cpus / stages.max(1)).max(1)
        };
        vec![
            "--kernel".into(),
            kernel.clone(),
            "--threads".into(),
            threads.to_string(),
        ]
    };

    // Hop latency: ring round trips with no compute.
    let stages_list: Vec<usize> = args.list("--stages", "1,2,4")?;
    let pings = args.number("--pings", 200)?;
    let payloads: Vec<usize> = args.list(
        "--payloads",
        &format!("0,{},14336,28672,57344", c.d_model * 8),
    )?;
    let mut hops = Vec::new();
    let mut throughput = Vec::new();
    for &stages in &stages_list {
        let cuts = even_cuts(c.n_layers, stages);
        let mut island =
            ProcessIsland::launch(&exe, &package_path, &cuts, &c, |_| stage_args(stages))?;
        for &payload in &payloads {
            for _ in 0..pings / 10 + 1 {
                island.coordinator.ping(payload)?;
            }
            let mut samples: Vec<f64> = (0..pings)
                .map(|_| island.coordinator.ping(payload).map(|s| s * 1e3))
                .collect::<Result<_, _>>()?;
            samples.sort_by(f64::total_cmp);
            let ring_hops = stages + 1;
            let row = json!({
                "stages": stages,
                "payload_bytes": payload,
                "ring_hops": ring_hops,
                "ring_median_ms": percentile(&samples, 0.5),
                "ring_p90_ms": percentile(&samples, 0.9),
                "ring_p99_ms": percentile(&samples, 0.99),
                "per_hop_median_ms": percentile(&samples, 0.5) / ring_hops as f64,
            });
            eprintln!("hop: {row}");
            hops.push(row);
        }
        // Throughput: single stream, then micro-batches filling the pipeline.
        let mut schedules = vec![(1usize, 1usize)];
        if stages > 1 {
            schedules.push((stages, stages));
        }
        schedules.push((stages.max(2), count));
        for (g, b) in schedules {
            let schedule = Schedule {
                micro_batches: g,
                concurrency: b,
                forget_finished: true,
                ..Schedule::default()
            };
            let (done, stats) = island.coordinator.run(&requests, &schedule)?;
            let exact = matches_reference(&done, &reference);
            if !exact {
                failures.push(format!(
                    "{stages} stages, G {g}, B {b}: differs from the single process"
                ));
            }
            let (mean, median) = answer_rates(&done);
            let row = json!({
                "stages": stages,
                "micro_batches": g,
                "concurrency": b,
                "requests": count,
                "generated_tokens": stats.generated_tokens,
                "seconds": stats.seconds,
                "aggregate_tok_s": stats.generated_tokens as f64 / stats.seconds,
                "per_answer_decode_tok_s_mean": mean,
                "per_answer_decode_tok_s_median": median,
                "bytes_sent_to_first_stage": stats.bytes_sent,
                "bit_exact_vs_single_process": exact,
            });
            eprintln!("throughput: {row}");
            throughput.push(row);
        }
        let stage_stats = island.shutdown()?;
        throughput.push(json!({"stages": stages, "stage_statistics": stage_stats}));
    }

    // WAN emulation sweep: per-answer and aggregate speed vs stage count and
    // micro-batch depth, with Kimi-sized activations on the wire.
    let mut wan = Vec::new();
    if !args.flag("--skip-wan") {
        let wan_stages: Vec<usize> = args.list("--wan-stages", "2,4,8")?;
        let wan_ms: Vec<f64> = args.list("--wan-ms", "10,30,60")?;
        let wan_mbit: Vec<f64> = args.list("--wan-mbit", "20,100")?;
        let depths: Vec<usize> = args.list("--wan-depths", "1,4,16")?;
        let jitter_frac = args.float("--wan-jitter-frac", 0.1)?;
        let wire_bytes = args.number("--wan-wire-bytes", 7168 * 4)?;
        let wan_tokens = args.number("--wan-max-tokens", 6)?;
        let wan_prompt = args.number("--wan-prompt-len", 2)?;
        let max_b = wan_stages.iter().max().copied().unwrap_or(1)
            * depths.iter().max().copied().unwrap_or(1);
        let wan_requests = bench_requests(&c, max_b, wan_prompt, wan_tokens);
        let wan_reference = reference_generations(&model, &wan_requests)?;
        // The real hidden vector is narrower than a Kimi boundary; pad each
        // position up to `wire_bytes` so the emulated uplink carries what a
        // Kimi-K2 stage would send.
        let hidden_bytes = c.d_model * 4;
        let pad = wire_bytes.saturating_sub(hidden_bytes) as u32;
        for &stages in &wan_stages {
            if stages > c.n_layers {
                continue;
            }
            let cuts = even_cuts(c.n_layers, stages);
            for &ms in &wan_ms {
                for &mbit in &wan_mbit {
                    let extra = |s: usize| {
                        let mut extra = stage_args(stages);
                        extra.extend([
                            "--wan-ms".into(),
                            ms.to_string(),
                            "--wan-jitter-ms".into(),
                            (ms * jitter_frac).to_string(),
                            "--wan-mbit".into(),
                            mbit.to_string(),
                            "--wan-seed".into(),
                            (s + 1).to_string(),
                        ]);
                        extra
                    };
                    let mut island = ProcessIsland::launch(&exe, &package_path, &cuts, &c, extra)?;
                    let timer = island.stages[0].hello["wan_timer_mode"].clone();
                    for &depth in &depths {
                        let b = stages * depth;
                        let schedule = Schedule {
                            micro_batches: stages,
                            concurrency: b,
                            pad_bytes_per_position: pad,
                            forget_finished: true,
                            ..Schedule::default()
                        };
                        let (done, stats) =
                            island.coordinator.run(&wan_requests[..b], &schedule)?;
                        let exact = matches_reference(&done, &wan_reference[..b]);
                        if !exact {
                            failures.push(format!(
                                "WAN {stages} stages {ms} ms {mbit} Mbit depth {depth}: differs"
                            ));
                        }
                        let (mean, median) = answer_rates(&done);
                        let row = json!({
                            "stages": stages,
                            "one_way_ms": ms,
                            "jitter_ms": ms * jitter_frac,
                            "uplink_mbit": mbit,
                            "wire_bytes_per_position": wire_bytes,
                            "micro_batches": stages,
                            "depth": depth,
                            "concurrency": b,
                            "generated_tokens": stats.generated_tokens,
                            "seconds": stats.seconds,
                            "aggregate_tok_s": stats.generated_tokens as f64 / stats.seconds,
                            "per_answer_decode_tok_s_mean": mean,
                            "per_answer_decode_tok_s_median": median,
                            "bit_exact_vs_single_process": exact,
                            "wan_timer_mode": timer,
                        });
                        eprintln!("wan: {row}");
                        wan.push(row);
                    }
                    island.shutdown()?;
                }
            }
        }
    }

    let report = json!({
        "schema": "arc.island-bench.v1",
        "label": label,
        "platform": platform(),
        "model": {"config": c.to_json(), "profile": c.profile(), "package": digest.to_json()},
        "requests": {"count": count, "prompt_len": prompt_len, "max_tokens": max_tokens},
        "compute": compute,
        "boundary_widths": widths,
        "hops": hops,
        "throughput": throughput,
        "wan": wan,
        "failures": failures,
        "seconds": started.elapsed().as_secs_f64(),
    });
    write_json(&args.path("--out")?, &report)?;
    if !failures.is_empty() {
        return Err(ModernError::Invalid(format!(
            "bit-exactness failures: {failures:?}"
        )));
    }
    Ok(())
}

fn main() -> ExitCode {
    let mut items: Vec<String> = std::env::args().skip(1).collect();
    if items.is_empty() {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    }
    let command = items.remove(0);
    let args = Args { items };
    let result = match command.as_str() {
        "synth" => cmd_synth(&args),
        "stage" => cmd_stage(&args),
        "experts" => cmd_experts(&args),
        "reference" => cmd_reference(&args),
        "run" => cmd_run(&args),
        "bench" => cmd_bench(&args),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("arc-island {command}: {error}");
            ExitCode::FAILURE
        }
    }
}
