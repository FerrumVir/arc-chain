use arc_gpu::metal_verify::{MetalVerifier, VerifyTask};

fn documented_usage() -> String {
    let mut verifier = MetalVerifier::new();
    let available = verifier.is_gpu_available();
    let tasks: Vec<VerifyTask> = Vec::new();
    let result = verifier.batch_verify(&tasks);
    format!(
        "GPU available: {available}; {} of {} valid, took {}us",
        result.valid, result.total, result.elapsed_us
    )
}

#[test]
fn documented_usage_compiles_without_touching_the_gpu() {
    let _ = std::hint::black_box(documented_usage as fn() -> String);
}
