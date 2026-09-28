use arc_finality_client::{
    MAX_BLOCK_JSON_BYTES, MAX_FINALITY_JSON_BYTES, MAX_TRUST_BYTES, verify_payloads,
};
use std::{env, fs::File, io::Read, path::PathBuf, process::ExitCode};

fn read_bounded(path: &PathBuf, limit: usize) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() > limit {
        return Err(format!("{} exceeds {} byte limit", path.display(), limit));
    }
    Ok(bytes)
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    if args.next().as_deref() != Some("verify") {
        return Err("usage: arc-finality-client verify --trust FILE --trust-sha256 HEX --host HOST --height N --finality FILE --block FILE".into());
    }
    let mut trust = None;
    let mut trust_sha256 = None;
    let mut host = None;
    let mut height = None;
    let mut finality = None;
    let mut block = None;
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        let slot = match flag.as_str() {
            "--trust" => &mut trust,
            "--trust-sha256" => &mut trust_sha256,
            "--host" => &mut host,
            "--height" => &mut height,
            "--finality" => &mut finality,
            "--block" => &mut block,
            _ => return Err(format!("unknown option {flag}")),
        };
        if slot.replace(value).is_some() {
            return Err(format!("duplicate option {flag}"));
        }
    }
    let trust_path = PathBuf::from(trust.ok_or("missing --trust")?);
    let trust_sha = trust_sha256.ok_or("missing --trust-sha256")?;
    let host = host.ok_or("missing --host")?;
    let height: u64 = height
        .ok_or("missing --height")?
        .parse()
        .map_err(|_| "--height must be an unsigned integer")?;
    let finality_path = PathBuf::from(finality.ok_or("missing --finality")?);
    let block_path = PathBuf::from(block.ok_or("missing --block")?);

    let trust_bytes = read_bounded(&trust_path, MAX_TRUST_BYTES)?;
    let finality_bytes = read_bounded(&finality_path, MAX_FINALITY_JSON_BYTES)?;
    let block_bytes = read_bounded(&block_path, MAX_BLOCK_JSON_BYTES)?;
    let result = verify_payloads(
        &trust_bytes,
        &trust_sha,
        &host,
        height,
        &finality_bytes,
        &block_bytes,
    )
    .map_err(|error| error.to_string())?;
    println!(
        "verified host={} height={} block={} state_root={} tx_root={} signing_stake={} quorum={} trust_sha256={} checkpoint_height={}",
        result.host,
        result.height,
        result.block_hash,
        result.state_root,
        result.tx_root,
        result.signing_stake,
        result.quorum,
        result.trust_sha256,
        result.checkpoint_height
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("verification failed: {error}");
            ExitCode::FAILURE
        }
    }
}
