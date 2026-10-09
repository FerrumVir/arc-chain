//! DeepSeek-V3-architecture models on ARC's integer engine: multi-head latent
//! attention (MLA), the mixture-of-experts layer, and pipeline stages.
//!
//! This module implements the profile
//! `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1`, specified in
//! `docs/protocol/integer-profile-mla-moe-dyadic-v1.md`. The development
//! model is Moonlight-16B-A3B-Instruct. Its architecture is the text
//! architecture of Kimi K2 (MLA, sigmoid routing with a correction bias, shared
//! experts), at 16B instead of 1T parameters.
//!
//! The profile reuses the dyadic v1 operators of [`super::arith`] (INT8 rows
//! with dyadic scales, exact RMS norm, two-pass exp, gated SiLU) and adds the
//! pieces below:
//!
//! * [`config`]: the model shape read from `config.json`, and the refusals;
//! * [`ops`]: interleaved RoPE, absorbed MLA attention, INT16 router rows,
//!   expert selection with index tie-breaking, Q32 routing weights, and the
//!   exact expert combine;
//! * [`package`]: stage packages (any contiguous layer range), segment
//!   digests, the layout-independent model root, and the stage manifest;
//! * [`convert`]: BF16 safetensors to stage packages, integer-only;
//! * [`model`]: the stage forward pass, generation, and teacher-forced stage
//!   runs;
//! * [`boundary`]: the serialised stage-boundary activations and their hashes.
//!
//! Nothing in consensus, rewards or native inference refers to this module.

pub mod boundary;
pub mod config;
pub mod convert;
/// Opt-in exact Metal GEMV for the INT16 projection.
#[cfg(all(feature = "metal-exact", target_os = "macos", target_arch = "aarch64"))]
pub mod metal_i16;
pub mod model;
pub mod ops;
pub mod package;
pub mod precision;
pub mod yarn;
mod yarn_constants;

/// Arithmetic profile implemented by this module.
pub const PROFILE: &str = "arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1";
/// The variant with INT4 group-32 routed experts (spec §13).
pub const PROFILE_I4G32: &str = "arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.i4g32-experts.q16.v1";
/// Stage package schema.
pub const STAGE_PACKAGE_SCHEMA: &str = "arc.integer-stage-package.v1";
/// Stage manifest schema.
pub const STAGE_MANIFEST_SCHEMA: &str = "arc.integer-stage-manifest.v1";
/// Boundary file schema.
pub const BOUNDARY_SCHEMA: &str = "arc.stage-boundary.v1";
/// Stage package file magic.
pub const STAGE_MAGIC: &[u8; 8] = b"ARCSPKG1";
/// Boundary file magic.
pub const BOUNDARY_MAGIC: &[u8; 8] = b"ARCBND01";
/// Golden run schema written by `arc-mla golden`.
pub const RUN_SCHEMA: &str = "arc.mla-run.v1";
/// Stage run report schema written by `arc-mla stage`.
pub const STAGE_RUN_SCHEMA: &str = "arc.mla-stage-run.v1";

#[cfg(test)]
mod tests {
    use super::super::identity_blake3;
    use super::*;

    #[test]
    fn the_profile_identity_matches_the_specification() {
        assert_eq!(
            identity_blake3(PROFILE),
            "7b0bd25616bd29195da71bb0b02d3c436350e801811280def1bb29c22d75207e"
        );
        assert_eq!(
            identity_blake3(PROFILE_I4G32),
            "5e6d6392186d817e57806184b1193ea1648805bb82cf7749f9d563306237e71c"
        );
        assert_eq!(
            identity_blake3(super::super::tiktoken::TOKENIZER_IDENTITY),
            "9506a8dc9c7806eae5eb7427be6056cb541ebbfcf1e9921915ce839e9d8a50c9"
        );
    }
}
