use crate::cached_integer_model::{I8Weights, I8WeightsView};
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
use crate::integer_lut::FRAC_BITS;
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
use rayon::prelude::*;
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

pub const LIMB_COUNT: usize = 4;
pub const LIMB_MAX: i64 = 2_139_062_143;
pub const LIMB_MIN: i64 = -2_155_905_152;
pub const MAX_COLS_FOR_I32: usize = 131_071;
pub const MAX_BATCH_TOKENS: usize = 1024;

const fn derive_limb_bound(digit: i64) -> i64 {
    let (mut acc, mut p, mut i) = (0i64, 1i64, 0usize);
    while i < LIMB_COUNT {
        acc += digit * p;
        p *= 256;
        i += 1;
    }
    acc
}
const _: () = assert!(LIMB_MAX == derive_limb_bound(127));
const _: () = assert!(LIMB_MIN == derive_limb_bound(-128));
const _: () = assert!(16_384i64 * (MAX_COLS_FOR_I32 as i64) <= i32::MAX as i64);
const _: () = assert!(16_384i64 * (MAX_COLS_FOR_I32 as i64 + 1) > i32::MAX as i64);
const _: () = assert!((MAX_COLS_FOR_I32 as i128) * 128 * (-LIMB_MIN as i128) < i64::MAX as i128);
const VNNI_GROUP_MAX: i64 = 4 * 255 * 128;
const _: () = assert!((MAX_COLS_FOR_I32.div_ceil(32) as i64) * VNNI_GROUP_MAX <= i32::MAX as i64);

pub const AVX2_ON_BY_DEFAULT: bool = true;

pub const NEON_ON_BY_DEFAULT: bool = true;

pub const KERNEL_ENV: &str = "ARC_CANONICAL_KERNEL";

pub const LEGACY_KERNEL_ENV: &str = "ARC_FAST_CANONICAL_KERNEL";

const STATE_UNRESOLVED: u8 = 0;
const MODE_SCALAR: u8 = 1;
const MODE_SIMD: u8 = 2;
const MODE_MASK: u8 = 0b11;
const SOURCE_DEFAULT: u8 = 0;
const SOURCE_ENV: u8 = 1;
const SOURCE_LEGACY_ENV: u8 = 2;
const SOURCE_EXPLICIT: u8 = 3;

static KERNEL_STATE: AtomicU8 = AtomicU8::new(STATE_UNRESOLVED);

const fn pack_state(simd: bool, source: u8) -> u8 {
    let mode = if simd { MODE_SIMD } else { MODE_SCALAR };
    (source << 2) | mode
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KernelChoice {
    simd: bool,
    source: u8,
    unrecognised: bool,
}

fn choose_kernel(
    requested: Option<&str>,
    legacy: Option<&str>,
    simd_by_default: bool,
) -> KernelChoice {
    if let Some(value) = requested.map(str::trim).filter(|value| !value.is_empty()) {
        let (simd, unrecognised) = if value.eq_ignore_ascii_case("auto") {
            (simd_by_default, false)
        } else if value.eq_ignore_ascii_case("simd") {
            (true, false)
        } else if value.eq_ignore_ascii_case("scalar") {
            (false, false)
        } else {
            (false, true)
        };
        return KernelChoice {
            simd,
            source: SOURCE_ENV,
            unrecognised,
        };
    }
    if let Some(value) = legacy {
        return KernelChoice {
            simd: value == "1",
            source: SOURCE_LEGACY_ENV,
            unrecognised: false,
        };
    }
    KernelChoice {
        simd: simd_by_default,
        source: SOURCE_DEFAULT,
        unrecognised: false,
    }
}

pub fn simd_on_by_default() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        AVX2_ON_BY_DEFAULT && !running_under_rosetta()
    }
    #[cfg(target_arch = "aarch64")]
    {
        NEON_ON_BY_DEFAULT
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        false
    }
}

#[cfg(target_arch = "x86_64")]
fn running_under_rosetta() -> bool {
    #[cfg(target_os = "macos")]
    {
        let mut translated: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        let status = unsafe {
            libc::sysctlbyname(
                c"sysctl.proc_translated".as_ptr(),
                std::ptr::addr_of_mut!(translated).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        status == 0 && translated == 1
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn kernel_state() -> u8 {
    match KERNEL_STATE.load(Ordering::Relaxed) {
        STATE_UNRESOLVED => resolve_kernel_state(),
        state => state,
    }
}

#[cold]
fn resolve_kernel_state() -> u8 {
    let requested = std::env::var(KERNEL_ENV).ok();
    let legacy = std::env::var(LEGACY_KERNEL_ENV).ok();
    let choice = choose_kernel(
        requested.as_deref(),
        legacy.as_deref(),
        simd_on_by_default(),
    );
    let state = pack_state(choice.simd, choice.source);
    match KERNEL_STATE.compare_exchange(
        STATE_UNRESOLVED,
        state,
        Ordering::Relaxed,
        Ordering::Relaxed,
    ) {
        Ok(_) => {
            if choice.unrecognised {
                tracing::warn!(
                    variable = KERNEL_ENV,
                    value = requested.as_deref().unwrap_or_default(),
                    "unrecognised kernel override (expected scalar, simd or auto); \
                     using the scalar reference kernel"
                );
            }
            state
        }
        Err(current) => current,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Unavailable,
    Shape,
    InnerDimAboveI32Bound,
    ActivationOutOfDomain,
    ScaleMultiplyWouldOverflow,
}

static CENSUS_ON: AtomicBool = AtomicBool::new(false);
static N_ATTEMPTED: AtomicU64 = AtomicU64::new(0);
static N_ACCEPTED: AtomicU64 = AtomicU64::new(0);
static N_UNAVAILABLE: AtomicU64 = AtomicU64::new(0);
static N_SHAPE: AtomicU64 = AtomicU64::new(0);
static N_K_BOUND: AtomicU64 = AtomicU64::new(0);
static N_DOMAIN: AtomicU64 = AtomicU64::new(0);
static N_SCALE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct ProjectionCensus {
    pub attempted: u64,
    pub accepted: u64,
    pub refused_unavailable: u64,
    pub refused_shape: u64,
    pub refused_inner_dim_above_i32_bound: u64,
    pub refused_activation_out_of_domain: u64,
    pub refused_scale_multiply_would_overflow: u64,
}

impl ProjectionCensus {
    pub fn refused_total(&self) -> u64 {
        self.refused_unavailable
            + self.refused_shape
            + self.refused_inner_dim_above_i32_bound
            + self.refused_activation_out_of_domain
            + self.refused_scale_multiply_would_overflow
    }
}

pub fn set_projection_census_enabled(on: bool) {
    CENSUS_ON.store(on, Ordering::Relaxed);
}

pub fn reset_projection_census() {
    for c in [
        &N_ATTEMPTED,
        &N_ACCEPTED,
        &N_UNAVAILABLE,
        &N_SHAPE,
        &N_K_BOUND,
        &N_DOMAIN,
        &N_SCALE,
    ] {
        c.store(0, Ordering::Relaxed);
    }
}

pub fn projection_census() -> ProjectionCensus {
    ProjectionCensus {
        attempted: N_ATTEMPTED.load(Ordering::Relaxed),
        accepted: N_ACCEPTED.load(Ordering::Relaxed),
        refused_unavailable: N_UNAVAILABLE.load(Ordering::Relaxed),
        refused_shape: N_SHAPE.load(Ordering::Relaxed),
        refused_inner_dim_above_i32_bound: N_K_BOUND.load(Ordering::Relaxed),
        refused_activation_out_of_domain: N_DOMAIN.load(Ordering::Relaxed),
        refused_scale_multiply_would_overflow: N_SCALE.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
thread_local! {
    static THREAD_CENSUS: std::cell::Cell<[u64; 3]> = const { std::cell::Cell::new([0; 3]) };
}

#[cfg(test)]
fn count_on_this_thread(slot: usize) {
    THREAD_CENSUS.with(|cell| {
        let mut counts = cell.get();
        counts[slot] += 1;
        cell.set(counts);
    });
}

#[cfg(test)]
pub(crate) fn take_thread_census() -> [u64; 3] {
    THREAD_CENSUS.with(|cell| cell.replace([0; 3]))
}

#[inline]
fn record_attempt() {
    #[cfg(test)]
    count_on_this_thread(0);
    if CENSUS_ON.load(Ordering::Relaxed) {
        N_ATTEMPTED.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline]
fn record_accept() {
    #[cfg(test)]
    count_on_this_thread(1);
    if CENSUS_ON.load(Ordering::Relaxed) {
        N_ACCEPTED.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline]
fn record_refusal(reason: Refusal) -> bool {
    #[cfg(test)]
    count_on_this_thread(2);
    if CENSUS_ON.load(Ordering::Relaxed) {
        match reason {
            Refusal::Unavailable => &N_UNAVAILABLE,
            Refusal::Shape => &N_SHAPE,
            Refusal::InnerDimAboveI32Bound => &N_K_BOUND,
            Refusal::ActivationOutOfDomain => &N_DOMAIN,
            Refusal::ScaleMultiplyWouldOverflow => &N_SCALE,
        }
        .fetch_add(1, Ordering::Relaxed);
    }
    false
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
thread_local! {
    static LIMB_SCRATCH: RefCell<Vec<i8>> = const { RefCell::new(Vec::new()) };
}

pub fn set_fast_canonical_kernel(enabled: bool) {
    KERNEL_STATE.store(pack_state(enabled, SOURCE_EXPLICIT), Ordering::Relaxed);
}

pub fn fast_canonical_kernel_enabled() -> bool {
    (kernel_state() & MODE_MASK) == MODE_SIMD && dotprod_available()
}

pub fn effective_kernel_name() -> &'static str {
    if !fast_canonical_kernel_enabled() {
        "scalar-i8xi64"
    } else if cfg!(target_arch = "x86_64") {
        "avx2-limb"
    } else {
        "neon-sdot-limb"
    }
}

pub fn kernel_choice_source() -> &'static str {
    match kernel_state() >> 2 {
        SOURCE_ENV => KERNEL_ENV,
        SOURCE_LEGACY_ENV => LEGACY_KERNEL_ENV,
        SOURCE_EXPLICIT => "explicit",
        _ => "default",
    }
}

pub fn dotprod_available() -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("dotprod")
    }
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BatchedKernel {
    NeonSdot,
    NeonI8mm,
    Avx2,
    AvxVnni,
    Avx512Vnni,
}

impl BatchedKernel {
    pub const ALL: [Self; 5] = [
        Self::NeonSdot,
        Self::NeonI8mm,
        Self::Avx2,
        Self::AvxVnni,
        Self::Avx512Vnni,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::NeonSdot => "neon-sdot-limb",
            Self::NeonI8mm => "neon-i8mm-limb",
            Self::Avx2 => "avx2-limb",
            Self::AvxVnni => "avx-vnni-limb",
            Self::Avx512Vnni => "avx512-vnni-limb",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kernel| kernel.label() == label)
    }

    pub const fn serves_one_row(self) -> bool {
        matches!(self, Self::NeonI8mm | Self::AvxVnni | Self::Avx512Vnni)
    }

    pub fn available(self) -> bool {
        match self {
            #[cfg(target_arch = "aarch64")]
            Self::NeonSdot => std::arch::is_aarch64_feature_detected!("dotprod"),
            #[cfg(target_arch = "aarch64")]
            Self::NeonI8mm => {
                std::arch::is_aarch64_feature_detected!("dotprod")
                    && std::arch::is_aarch64_feature_detected!("i8mm")
            }
            #[cfg(target_arch = "x86_64")]
            Self::Avx2 => std::arch::is_x86_feature_detected!("avx2"),
            #[cfg(target_arch = "x86_64")]
            Self::AvxVnni => {
                std::arch::is_x86_feature_detected!("avx2")
                    && std::arch::is_x86_feature_detected!("avxvnni")
            }
            #[cfg(target_arch = "x86_64")]
            Self::Avx512Vnni => {
                std::arch::is_x86_feature_detected!("avx2")
                    && std::arch::is_x86_feature_detected!("avx512f")
                    && std::arch::is_x86_feature_detected!("avx512vnni")
            }
            _ => false,
        }
    }

    pub const fn required_cpu_features(self) -> &'static [&'static str] {
        match self {
            Self::NeonSdot => &["dotprod"],
            Self::NeonI8mm => &["dotprod", "i8mm"],
            Self::Avx2 => &["avx2"],
            Self::AvxVnni => &["avx2", "avxvnni"],
            Self::Avx512Vnni => &["avx2", "avx512f", "avx512vnni"],
        }
    }

    pub fn missing_cpu_features(self) -> Vec<&'static str> {
        let detected = detected_cpu_features();
        self.required_cpu_features()
            .iter()
            .copied()
            .filter(|feature| !detected.contains(&(*feature, true)))
            .collect()
    }
}

const BASE_KERNEL: BatchedKernel = if cfg!(target_arch = "x86_64") {
    BatchedKernel::Avx2
} else {
    BatchedKernel::NeonSdot
};

const fn one_row_kernel(selected: BatchedKernel) -> BatchedKernel {
    if selected.serves_one_row() {
        selected
    } else {
        BASE_KERNEL
    }
}

pub fn available_batched_kernels() -> Vec<BatchedKernel> {
    BatchedKernel::ALL
        .into_iter()
        .filter(|kernel| kernel.available())
        .collect()
}

pub fn best_batched_kernel() -> Option<BatchedKernel> {
    [
        BatchedKernel::Avx512Vnni,
        BatchedKernel::AvxVnni,
        BatchedKernel::Avx2,
        BatchedKernel::NeonI8mm,
        BatchedKernel::NeonSdot,
    ]
    .into_iter()
    .find(|kernel| kernel.available())
}

static BATCHED_PREFERENCE: AtomicU8 = AtomicU8::new(0);

pub const BATCHED_KERNEL_ENV: &str = "ARC_CANONICAL_BATCHED_KERNEL";

fn apply_batched_kernel_env() -> Option<&'static str> {
    static ENV_VALUE: OnceLock<Option<String>> = OnceLock::new();
    ENV_VALUE
        .get_or_init(|| {
            let value = std::env::var(BATCHED_KERNEL_ENV).ok()?.trim().to_owned();
            match BatchedKernel::from_label(&value) {
                Some(kernel) => BATCHED_PREFERENCE.store(kernel as u8 + 1, Ordering::Relaxed),
                None if value.is_empty() || value == "auto" => {}
                None => eprintln!(
                    "ARC_CANONICAL_BATCHED_KERNEL={value:?} names no batched kernel; selecting automatically"
                ),
            }
            Some(value)
        })
        .as_deref()
}

fn names_no_kernel(value: &str) -> bool {
    !value.is_empty() && value != "auto" && BatchedKernel::from_label(value).is_none()
}

pub fn set_batched_kernel_preference(kernel: Option<BatchedKernel>) {
    apply_batched_kernel_env();
    BATCHED_PREFERENCE.store(kernel.map_or(0, |k| k as u8 + 1), Ordering::Relaxed);
}

pub fn batched_kernel_preference() -> Option<BatchedKernel> {
    apply_batched_kernel_env();
    match BATCHED_PREFERENCE.load(Ordering::Relaxed) {
        0 => None,
        slot => BatchedKernel::ALL.get(usize::from(slot) - 1).copied(),
    }
}

pub fn selected_batched_kernel() -> Option<BatchedKernel> {
    match batched_kernel_preference() {
        Some(kernel) if kernel.available() => Some(kernel),
        _ => best_batched_kernel(),
    }
}

static BATCHED_RUNS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
static ONE_ROW_RUNS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
static LAST_BATCHED: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
thread_local! {
    static THREAD_KERNEL_RUNS: std::cell::Cell<[[u64; 5]; 2]> =
        const { std::cell::Cell::new([[0; 5]; 2]) };
}

#[cfg(all(test, any(target_arch = "aarch64", target_arch = "x86_64")))]
fn count_kernel_on_this_thread(path: usize, kernel: BatchedKernel) {
    THREAD_KERNEL_RUNS.with(|cell| {
        let mut runs = cell.get();
        runs[path][kernel as usize] += 1;
        cell.set(runs);
    });
}

#[cfg(test)]
fn take_thread_kernel_runs() -> [[u64; 5]; 2] {
    THREAD_KERNEL_RUNS.with(|cell| cell.replace([[0; 5]; 2]))
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn record_batched_run(kernel: BatchedKernel) {
    BATCHED_RUNS[kernel as usize].fetch_add(1, Ordering::Relaxed);
    LAST_BATCHED.store(kernel as u8 + 1, Ordering::Relaxed);
    #[cfg(test)]
    count_kernel_on_this_thread(1, kernel);
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn record_one_row_run(kernel: BatchedKernel) {
    ONE_ROW_RUNS[kernel as usize].fetch_add(1, Ordering::Relaxed);
    #[cfg(test)]
    count_kernel_on_this_thread(0, kernel);
}

pub fn batched_kernel_runs(kernel: BatchedKernel) -> u64 {
    BATCHED_RUNS[kernel as usize].load(Ordering::Relaxed)
}

pub fn one_row_kernel_runs(kernel: BatchedKernel) -> u64 {
    ONE_ROW_RUNS[kernel as usize].load(Ordering::Relaxed)
}

pub fn last_batched_kernel() -> Option<BatchedKernel> {
    match LAST_BATCHED.load(Ordering::Relaxed) {
        0 => None,
        slot => BatchedKernel::ALL.get(usize::from(slot) - 1).copied(),
    }
}

pub fn batched_kernel_run_report() -> String {
    BatchedKernel::ALL
        .into_iter()
        .filter(|kernel| kernel.available() || batched_kernel_runs(*kernel) > 0)
        .map(|kernel| format!("{}={}", kernel.label(), batched_kernel_runs(kernel)))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn detected_cpu_features() -> Vec<(&'static str, bool)> {
    #[cfg(target_arch = "aarch64")]
    {
        vec![
            (
                "dotprod",
                std::arch::is_aarch64_feature_detected!("dotprod"),
            ),
            ("i8mm", std::arch::is_aarch64_feature_detected!("i8mm")),
        ]
    }
    #[cfg(target_arch = "x86_64")]
    {
        vec![
            ("avx2", std::arch::is_x86_feature_detected!("avx2")),
            ("avx512f", std::arch::is_x86_feature_detected!("avx512f")),
            (
                "avx512vnni",
                std::arch::is_x86_feature_detected!("avx512vnni"),
            ),
            ("avxvnni", std::arch::is_x86_feature_detected!("avxvnni")),
        ]
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        Vec::new()
    }
}

pub const SCALAR_KERNEL_LABEL: &str = "scalar-i8xi64";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinStatus {
    Unpinned,
    Honoured,
    FellBack,
    Inactive,
    Unrecognised,
}

impl PinStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unpinned => "unpinned",
            Self::Honoured => "honoured",
            Self::FellBack => "fell-back",
            Self::Inactive => "inactive",
            Self::Unrecognised => "unrecognised",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelReport {
    pub vector_enabled: bool,
    pub one_token: Option<BatchedKernel>,
    pub batched: Option<BatchedKernel>,
    pub pin: Option<BatchedKernel>,
    pub pin_env: Option<&'static str>,
    pub pin_status: PinStatus,
    pub pin_missing_cpu_features: Vec<&'static str>,
}

impl KernelReport {
    pub fn one_token_label(&self) -> &'static str {
        kernel_label(self.one_token)
    }

    pub fn batched_label(&self) -> &'static str {
        kernel_label(self.batched)
    }
}

fn kernel_label(kernel: Option<BatchedKernel>) -> &'static str {
    kernel.map_or(SCALAR_KERNEL_LABEL, BatchedKernel::label)
}

impl std::fmt::Display for KernelReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "one-token {}, batched {}",
            self.one_token_label(),
            self.batched_label()
        )?;
        let pin = kernel_label(self.pin);
        match self.pin_status {
            PinStatus::Unpinned => f.write_str("; no pin"),
            PinStatus::Honoured => write!(f, "; pin {pin} honoured"),
            PinStatus::FellBack => write!(
                f,
                "; pin {pin} fell back, this CPU lacks {}",
                self.pin_missing_cpu_features.join(", ")
            ),
            PinStatus::Inactive => write!(f, "; pin {pin} inactive, the vector kernels are off"),
            PinStatus::Unrecognised => write!(
                f,
                "; ARC_CANONICAL_BATCHED_KERNEL={:?} names no kernel",
                self.pin_env.unwrap_or_default()
            ),
        }
    }
}

pub fn kernel_report() -> KernelReport {
    let pin_env = apply_batched_kernel_env();
    let pin = batched_kernel_preference();
    let vector_enabled = fast_canonical_kernel_enabled();
    let selected = selected_batched_kernel().filter(|_| vector_enabled);
    let pin_status = match pin {
        Some(_) if !vector_enabled => PinStatus::Inactive,
        Some(kernel) if kernel.available() => PinStatus::Honoured,
        Some(_) => PinStatus::FellBack,
        None if pin_env.is_some_and(names_no_kernel) => PinStatus::Unrecognised,
        None => PinStatus::Unpinned,
    };
    KernelReport {
        vector_enabled,
        one_token: selected.map(one_row_kernel),
        batched: selected,
        pin,
        pin_env,
        pin_status,
        pin_missing_cpu_features: pin.map_or_else(Vec::new, BatchedKernel::missing_cpu_features),
    }
}

pub fn limb_bounds_match_formula() -> bool {
    let (mut hi, mut lo, mut p) = (0i64, 0i64, 1i64);
    for _ in 0..LIMB_COUNT {
        hi += 127 * p;
        lo += -128 * p;
        p *= 256;
    }
    hi == LIMB_MAX && lo == LIMB_MIN
}

pub fn split_limbs(input: &[i64], limbs: &mut [i8]) -> Option<usize> {
    let len = input.len();
    if len == 0 || limbs.len() < LIMB_COUNT * len {
        return None;
    }
    let mut used = 1usize;
    for (j, &x) in input.iter().enumerate() {
        if !(LIMB_MIN..=LIMB_MAX).contains(&x) {
            return None;
        }
        let mut u = x as u64;
        for i in 0..LIMB_COUNT {
            let low = (u & 255) as i32;
            let c = if low > 127 { low - 256 } else { low };
            limbs[i * len + j] = c as i8;
            u = u.wrapping_sub(c as i64 as u64) >> 8;
            if c != 0 && i + 1 > used {
                used = i + 1;
            }
        }
        let mut r = 0i64;
        for i in (0..LIMB_COUNT).rev() {
            r = r * 256 + limbs[i * len + j] as i64;
        }
        if r != x {
            return None;
        }
    }
    Some(used)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon,dotprod")]
#[inline]
unsafe fn sdot(
    acc: std::arch::aarch64::int32x4_t,
    a: std::arch::aarch64::int8x16_t,
    b: std::arch::aarch64::int8x16_t,
) -> std::arch::aarch64::int32x4_t {
    unsafe {
        let mut out = acc;
        std::arch::asm!(
            "sdot {o:v}.4s, {a:v}.16b, {b:v}.16b",
            o = inout(vreg) out,
            a = in(vreg) a,
            b = in(vreg) b,
            options(pure, nomem, nostack)
        );
        out
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon,dotprod")]
unsafe fn dot_limbs_dotprod(row: *const i8, limbs: &[i8], len: usize, used: usize) -> i64 {
    unsafe {
        use std::arch::aarch64::*;
        let mut total: i64 = 0;
        for i in 0..used {
            let plane = limbs.as_ptr().add(i * len);
            let mut acc0 = vdupq_n_s32(0);
            let mut acc1 = vdupq_n_s32(0);
            let mut j = 0usize;
            while j + 32 <= len {
                acc0 = sdot(acc0, vld1q_s8(row.add(j)), vld1q_s8(plane.add(j)));
                acc1 = sdot(acc1, vld1q_s8(row.add(j + 16)), vld1q_s8(plane.add(j + 16)));
                j += 32;
            }
            let mut a = vaddvq_s32(vaddq_s32(acc0, acc1)) as i64;
            while j < len {
                a += (*row.add(j) as i64) * (*plane.add(j) as i64);
                j += 1;
            }
            total += a * (1i64 << (8 * i));
        }
        total
    }
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn post_scale_bound_holds(input: &[i64], scales: &[i64]) -> bool {
    let Some(input_abs_sum) = input
        .iter()
        .try_fold(0i64, |sum, v| sum.checked_add(v.checked_abs()?))
    else {
        return false;
    };
    let Some(dot_bound) = input_abs_sum.checked_mul(128) else {
        return false;
    };
    scales.iter().all(|s| {
        s.checked_abs()
            .and_then(|a| dot_bound.checked_mul(a))
            .is_some()
    })
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[derive(Clone, Copy)]
struct SendPtr(*mut i64);
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
unsafe impl Send for SendPtr {}
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
unsafe impl Sync for SendPtr {}
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
impl SendPtr {
    #[inline]
    fn get(self) -> *mut i64 {
        self.0
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon,dotprod")]
unsafe fn dot_limbs_x4(
    row: *const i8,
    planes: &[*const i8; 4],
    len: usize,
    used: usize,
) -> [i64; 4] {
    unsafe {
        use std::arch::aarch64::*;
        let mut total = [0i64; 4];
        for l in 0..used {
            let off = l * len;
            let mut a0 = [vdupq_n_s32(0); 4];
            let mut a1 = [vdupq_n_s32(0); 4];
            let mut j = 0usize;
            while j + 32 <= len {
                let w0 = vld1q_s8(row.add(j));
                let w1 = vld1q_s8(row.add(j + 16));
                for q in 0..4 {
                    let c = planes[q].add(off + j);
                    a0[q] = sdot(a0[q], w0, vld1q_s8(c));
                    a1[q] = sdot(a1[q], w1, vld1q_s8(c.add(16)));
                }
                j += 32;
            }
            for q in 0..4 {
                let mut acc = vaddvq_s32(vaddq_s32(a0[q], a1[q])) as i64;
                let c = planes[q].add(off);
                let mut jj = j;
                while jj < len {
                    acc += (*row.add(jj) as i64) * (*c.add(jj) as i64);
                    jj += 1;
                }
                total[q] += acc * (1i64 << (8 * l));
            }
        }
        total
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_limbs_avx2<const N: usize>(
    row: *const i8,
    planes: &[*const i8; N],
    len: usize,
    used: usize,
) -> [i64; N] {
    unsafe {
        use std::arch::x86_64::*;
        let mut total = [0i64; N];
        for limb in 0..used {
            let mut accumulators = [_mm256_setzero_si256(); N];
            let mut j = 0usize;
            while j + 16 <= len {
                let weights = _mm256_cvtepi8_epi16(_mm_loadu_si128(row.add(j).cast()));
                for q in 0..N {
                    let digits =
                        _mm256_cvtepi8_epi16(_mm_loadu_si128(planes[q].add(limb * len + j).cast()));
                    accumulators[q] =
                        _mm256_add_epi32(accumulators[q], _mm256_madd_epi16(weights, digits));
                }
                j += 16;
            }
            for q in 0..N {
                let mut lanes = [0i32; 8];
                _mm256_storeu_si256(lanes.as_mut_ptr().cast(), accumulators[q]);
                let mut partial: i64 = lanes.iter().map(|&lane| i64::from(lane)).sum();
                for tail in j..len {
                    partial +=
                        i64::from(*row.add(tail)) * i64::from(*planes[q].add(limb * len + tail));
                }
                total[q] += partial * (1i64 << (8 * limb));
            }
        }
        total
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_limbs_dotprod(row: *const i8, limbs: &[i8], len: usize, used: usize) -> i64 {
    unsafe { dot_limbs_avx2(row, &[limbs.as_ptr()], len, used)[0] }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_limbs_x4(
    row: *const i8,
    planes: &[*const i8; 4],
    len: usize,
    used: usize,
) -> [i64; 4] {
    unsafe { dot_limbs_avx2(row, planes, len, used) }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon,i8mm")]
#[inline]
unsafe fn smmla(
    acc: std::arch::aarch64::int32x4_t,
    a: std::arch::aarch64::int8x16_t,
    b: std::arch::aarch64::int8x16_t,
) -> std::arch::aarch64::int32x4_t {
    unsafe {
        let mut out = acc;
        std::arch::asm!(
            "smmla {o:v}.4s, {a:v}.16b, {b:v}.16b",
            o = inout(vreg) out,
            a = in(vreg) a,
            b = in(vreg) b,
            options(pure, nomem, nostack)
        );
        out
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon,i8mm")]
unsafe fn i8mm_tile<const TP: usize>(
    rows: &[[*const i8; 2]; 2],
    pairs: &[*const i8; TP],
    len: usize,
    used: usize,
) -> [[[i64; 4]; TP]; 2] {
    unsafe {
        use std::arch::aarch64::*;
        let mut total = [[[0i64; 4]; TP]; 2];
        for l in 0..used {
            let plane = l * 2 * len;
            let mut acc = [[vdupq_n_s32(0); TP]; 2];
            let mut j = 0usize;
            while j + 16 <= len {
                let mut low = [vdupq_n_s8(0); 2];
                let mut high = [vdupq_n_s8(0); 2];
                for ((low, high), [a, b]) in low.iter_mut().zip(high.iter_mut()).zip(rows) {
                    let a = vreinterpretq_s64_s8(vld1q_s8(a.add(j)));
                    let b = vreinterpretq_s64_s8(vld1q_s8(b.add(j)));
                    *low = vreinterpretq_s8_s64(vzip1q_s64(a, b));
                    *high = vreinterpretq_s8_s64(vzip2q_s64(a, b));
                }
                for (tp, pair) in pairs.iter().enumerate() {
                    let digits = pair.add(plane + 2 * j);
                    let first = vld1q_s8(digits);
                    let second = vld1q_s8(digits.add(16));
                    for rp in 0..2 {
                        acc[rp][tp] = smmla(acc[rp][tp], low[rp], first);
                        acc[rp][tp] = smmla(acc[rp][tp], high[rp], second);
                    }
                }
                j += 16;
            }
            if j < len {
                let mut both = [vdupq_n_s8(0); 2];
                for (both, [a, b]) in both.iter_mut().zip(rows) {
                    *both = vcombine_s8(vld1_s8(a.add(j)), vld1_s8(b.add(j)));
                }
                for (tp, pair) in pairs.iter().enumerate() {
                    let digits = vld1q_s8(pair.add(plane + 2 * j));
                    for rp in 0..2 {
                        acc[rp][tp] = smmla(acc[rp][tp], both[rp], digits);
                    }
                }
            }
            let place = 1i64 << (8 * l);
            for (total, acc) in total.iter_mut().zip(&acc) {
                for (total, acc) in total.iter_mut().zip(acc) {
                    let mut lanes = [0i32; 4];
                    vst1q_s32(lanes.as_mut_ptr(), *acc);
                    for (total, lane) in total.iter_mut().zip(lanes) {
                        *total += i64::from(lane) * place;
                    }
                }
            }
        }
        total
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
#[inline]
fn lanes_sum_512(v: std::arch::x86_64::__m512i) -> i64 {
    use std::arch::x86_64::*;
    _mm512_reduce_add_epi64(_mm512_add_epi64(
        _mm512_cvtepi32_epi64(_mm512_castsi512_si256(v)),
        _mm512_cvtepi32_epi64(_mm512_extracti64x4_epi64::<1>(v)),
    ))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
fn lanes_sum_256(v: std::arch::x86_64::__m256i) -> i64 {
    use std::arch::x86_64::*;
    let wide = _mm256_add_epi64(
        _mm256_cvtepi32_epi64(_mm256_castsi256_si128(v)),
        _mm256_cvtepi32_epi64(_mm256_extracti128_si256::<1>(v)),
    );
    let pair = _mm_add_epi64(
        _mm256_castsi256_si128(wide),
        _mm256_extracti128_si256::<1>(wide),
    );
    _mm_extract_epi64::<0>(pair) + _mm_extract_epi64::<1>(pair)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512vnni")]
unsafe fn vnni512_tile<const T: usize>(
    rows: &[*const i8; 4],
    tokens: &[*const i8; T],
    in_size: usize,
    len: usize,
    used: usize,
    sums: &mut [i64; 4],
    fresh: bool,
) -> [[i64; T]; 4] {
    unsafe {
        use std::arch::x86_64::*;
        let ones = _mm512_set1_epi8(1);
        let mut row_sums = [_mm512_setzero_si512(); 4];
        let mut total = [[0i64; T]; 4];
        for l in 0..used {
            let summing = fresh && l == 0;
            let mut acc = [[_mm512_setzero_si512(); T]; 4];
            let mut j = 0usize;
            while j + 64 <= len {
                let mut weights = [_mm512_setzero_si512(); 4];
                for (weights, row) in weights.iter_mut().zip(rows) {
                    *weights = _mm512_loadu_si512(row.add(j).cast());
                }
                if summing {
                    for r in 0..4 {
                        row_sums[r] = _mm512_dpbusd_epi32(row_sums[r], ones, weights[r]);
                    }
                }
                for (t, token) in tokens.iter().enumerate() {
                    let digits = _mm512_loadu_si512(token.add(l * in_size + j).cast());
                    for r in 0..4 {
                        acc[r][t] = _mm512_dpbusd_epi32(acc[r][t], digits, weights[r]);
                    }
                }
                j += 64;
            }
            if summing {
                for (sum, lanes) in sums.iter_mut().zip(row_sums) {
                    *sum = lanes_sum_512(lanes);
                }
            }
            let place = 1i64 << (8 * l);
            for ((total, acc), &sum) in total.iter_mut().zip(&acc).zip(sums.iter()) {
                for (total, &acc) in total.iter_mut().zip(acc) {
                    *total += (lanes_sum_512(acc) - 128 * sum) * place;
                }
            }
        }
        total
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,avxvnni")]
unsafe fn vnni256_tile<const T: usize>(
    rows: &[*const i8; 2],
    tokens: &[*const i8; T],
    in_size: usize,
    len: usize,
    used: usize,
    sums: &mut [i64; 2],
    fresh: bool,
) -> [[i64; T]; 2] {
    unsafe {
        use std::arch::x86_64::*;
        let ones = _mm256_set1_epi8(1);
        let mut row_sums = [_mm256_setzero_si256(); 2];
        let mut total = [[0i64; T]; 2];
        for l in 0..used {
            let summing = fresh && l == 0;
            let mut acc = [[_mm256_setzero_si256(); T]; 2];
            let mut j = 0usize;
            while j + 32 <= len {
                let mut weights = [_mm256_setzero_si256(); 2];
                for (weights, row) in weights.iter_mut().zip(rows) {
                    *weights = _mm256_loadu_si256(row.add(j).cast());
                }
                if summing {
                    for r in 0..2 {
                        row_sums[r] = _mm256_dpbusd_avx_epi32(row_sums[r], ones, weights[r]);
                    }
                }
                for (t, token) in tokens.iter().enumerate() {
                    let digits = _mm256_loadu_si256(token.add(l * in_size + j).cast());
                    for r in 0..2 {
                        acc[r][t] = _mm256_dpbusd_avx_epi32(acc[r][t], digits, weights[r]);
                    }
                }
                j += 32;
            }
            if summing {
                for (sum, lanes) in sums.iter_mut().zip(row_sums) {
                    *sum = lanes_sum_256(lanes);
                }
            }
            let place = 1i64 << (8 * l);
            for ((total, acc), &sum) in total.iter_mut().zip(&acc).zip(sums.iter()) {
                for (total, &acc) in total.iter_mut().zip(acc) {
                    *total += (lanes_sum_256(acc) - 128 * sum) * place;
                }
            }
        }
        total
    }
}

pub fn matmul_i8_canonical_rows_fast(
    weights: &I8Weights,
    input: &[i64],
    in_size: usize,
    output: &mut [i64],
) -> bool {
    matmul_i8_canonical_rows_fast_view(I8WeightsView::from(weights), input, in_size, output)
}

pub(crate) fn matmul_i8_canonical_rows_fast_view(
    weights: I8WeightsView<'_>,
    input: &[i64],
    in_size: usize,
    output: &mut [i64],
) -> bool {
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = (weights, input, in_size, output);
        record_attempt();
        record_refusal(Refusal::Unavailable)
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        if let Some(kernel) = selected_batched_kernel().filter(|k| k.serves_one_row()) {
            let accepted = matmul_i8_batched_view(kernel, weights, input, 1, in_size, output);
            if accepted {
                record_one_row_run(kernel);
            }
            return accepted;
        }
        record_attempt();
        if !dotprod_available() {
            return record_refusal(Refusal::Unavailable);
        }
        if in_size == 0
            || input.len() != in_size
            || weights.n_cols != in_size
            || weights.n_rows == 0
            || weights.data.len() != weights.n_rows.saturating_mul(in_size)
            || weights.scales.len() != weights.n_rows
            || output.len() != weights.n_rows
        {
            return record_refusal(Refusal::Shape);
        }
        if in_size > MAX_COLS_FOR_I32 {
            return record_refusal(Refusal::InnerDimAboveI32Bound);
        }
        if !post_scale_bound_holds(input, weights.scales) {
            return record_refusal(Refusal::ScaleMultiplyWouldOverflow);
        }
        LIMB_SCRATCH.with(|cell| {
            let mut scratch = cell.borrow_mut();
            if scratch.len() < LIMB_COUNT * in_size {
                scratch.resize(LIMB_COUNT * in_size, 0);
            }
            let Some(used) = split_limbs(input, &mut scratch[..LIMB_COUNT * in_size]) else {
                return record_refusal(Refusal::ActivationOutOfDomain);
            };
            let data = &weights.data;
            let scales = &weights.scales;
            let limbs = &scratch[..LIMB_COUNT * in_size];
            output
                .par_chunks_mut(256)
                .enumerate()
                .for_each(|(chunk_idx, chunk)| {
                    let start = chunk_idx * 256;
                    for (local_i, out) in chunk.iter_mut().enumerate() {
                        let i = start + local_i;
                        let acc = unsafe {
                            dot_limbs_dotprod(data.as_ptr().add(i * in_size), limbs, in_size, used)
                        };
                        *out = (acc * scales[i]) >> FRAC_BITS;
                    }
                });
            record_accept();
            record_one_row_run(BASE_KERNEL);
            true
        })
    }
}

pub(crate) fn exact_row_dots_fast(
    data: &[i8],
    n_rows: usize,
    n_cols: usize,
    input: &[i64],
    acc: &mut [i64],
) -> bool {
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = (data, n_rows, n_cols, input, acc);
        record_attempt();
        record_refusal(Refusal::Unavailable)
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        record_attempt();
        if !dotprod_available() {
            return record_refusal(Refusal::Unavailable);
        }
        if n_cols == 0
            || n_rows == 0
            || input.len() != n_cols
            || data.len() != n_rows.saturating_mul(n_cols)
            || acc.len() != n_rows
        {
            return record_refusal(Refusal::Shape);
        }
        if n_cols > MAX_COLS_FOR_I32 {
            return record_refusal(Refusal::InnerDimAboveI32Bound);
        }
        LIMB_SCRATCH.with(|cell| {
            let mut scratch = cell.borrow_mut();
            if scratch.len() < LIMB_COUNT * n_cols {
                scratch.resize(LIMB_COUNT * n_cols, 0);
            }
            let Some(used) = split_limbs(input, &mut scratch[..LIMB_COUNT * n_cols]) else {
                return record_refusal(Refusal::ActivationOutOfDomain);
            };
            let limbs = &scratch[..LIMB_COUNT * n_cols];
            acc.par_chunks_mut(64)
                .enumerate()
                .for_each(|(chunk_idx, chunk)| {
                    let start = chunk_idx * 64;
                    for (local_i, out) in chunk.iter_mut().enumerate() {
                        let i = start + local_i;
                        *out = unsafe {
                            dot_limbs_dotprod(data.as_ptr().add(i * n_cols), limbs, n_cols, used)
                        };
                    }
                });
            record_accept();
            true
        })
    }
}

pub fn matmul_i8_batched_fast(
    weights: &I8Weights,
    inputs: &[i64],
    n_tokens: usize,
    in_size: usize,
    output: &mut [i64],
) -> bool {
    let Some(kernel) = selected_batched_kernel() else {
        record_attempt();
        return record_refusal(Refusal::Unavailable);
    };
    matmul_i8_batched_with(kernel, weights, inputs, n_tokens, in_size, output)
}

pub fn matmul_i8_batched_with(
    kernel: BatchedKernel,
    weights: &I8Weights,
    inputs: &[i64],
    n_tokens: usize,
    in_size: usize,
    output: &mut [i64],
) -> bool {
    matmul_i8_batched_view(
        kernel,
        I8WeightsView::from(weights),
        inputs,
        n_tokens,
        in_size,
        output,
    )
}

pub(crate) fn matmul_i8_batched_view(
    kernel: BatchedKernel,
    weights: I8WeightsView<'_>,
    inputs: &[i64],
    n_tokens: usize,
    in_size: usize,
    output: &mut [i64],
) -> bool {
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = (kernel, weights, inputs, n_tokens, in_size, output);
        record_attempt();
        record_refusal(Refusal::Unavailable)
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        record_attempt();
        if !kernel.available() {
            return record_refusal(Refusal::Unavailable);
        }
        if in_size == 0
            || n_tokens == 0
            || n_tokens > MAX_BATCH_TOKENS
            || inputs.len() != n_tokens.saturating_mul(in_size)
            || weights.n_cols != in_size
            || weights.n_rows == 0
            || weights.data.len() != weights.n_rows.saturating_mul(in_size)
            || weights.scales.len() != weights.n_rows
            || output.len() != n_tokens.saturating_mul(weights.n_rows)
        {
            return record_refusal(Refusal::Shape);
        }
        if in_size > MAX_COLS_FOR_I32 {
            return record_refusal(Refusal::InnerDimAboveI32Bound);
        }
        let scales = weights.scales;
        if !inputs
            .par_chunks(in_size)
            .all(|input| post_scale_bound_holds(input, scales))
        {
            return record_refusal(Refusal::ScaleMultiplyWouldOverflow);
        }
        let mut limbs = vec![0i8; n_tokens * LIMB_COUNT * in_size];
        let used: Option<Vec<usize>> = limbs
            .par_chunks_mut(LIMB_COUNT * in_size)
            .zip(inputs.par_chunks(in_size))
            .map(|(planes, input)| split_limbs(input, planes))
            .collect();
        let Some(used) = used else {
            return record_refusal(Refusal::ActivationOutOfDomain);
        };
        let job = BatchJob {
            data: weights.data,
            scales,
            inputs,
            n_rows: weights.n_rows,
            n_tokens,
            in_size,
            used: used.iter().copied().max().unwrap_or(1),
        };
        let ran = run_batched(kernel, job, &mut limbs, SendPtr(output.as_mut_ptr()));
        record_batched_run(ran);
        record_accept();
        true
    }
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[derive(Clone, Copy)]
struct BatchJob<'a> {
    data: &'a [i8],
    scales: &'a [i64],
    inputs: &'a [i64],
    n_rows: usize,
    n_tokens: usize,
    in_size: usize,
    used: usize,
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn tail_dot(weights: &[i8], inputs: &[i64]) -> i64 {
    weights
        .iter()
        .zip(inputs)
        .map(|(&w, &x)| i64::from(w) * x)
        .sum()
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn widen<E: Copy + Default, const R: usize, const T: usize>(tile: [[E; T]; R]) -> [[E; 4]; R] {
    let mut wide = [[E::default(); 4]; R];
    for (wide, tile) in wide.iter_mut().zip(tile) {
        wide[..T].copy_from_slice(&tile);
    }
    wide
}

#[cfg(target_arch = "aarch64")]
fn run_batched(
    kernel: BatchedKernel,
    job: BatchJob<'_>,
    limbs: &mut [i8],
    out: SendPtr,
) -> BatchedKernel {
    if kernel == BatchedKernel::NeonI8mm {
        run_i8mm(job, limbs, out);
        return BatchedKernel::NeonI8mm;
    }
    run_quads(job, limbs, out);
    BatchedKernel::NeonSdot
}

#[cfg(target_arch = "x86_64")]
fn run_batched(
    kernel: BatchedKernel,
    job: BatchJob<'_>,
    limbs: &mut [i8],
    out: SendPtr,
) -> BatchedKernel {
    let (in_size, used) = (job.in_size, job.used);
    match kernel {
        BatchedKernel::Avx512Vnni => {
            offset_digits(job, limbs);
            run_vnni::<4, _>(
                job,
                limbs,
                64,
                out,
                |rows, tokens, len, sums, fresh| unsafe {
                    match *tokens {
                        [a] => widen(vnni512_tile(rows, &[a], in_size, len, used, sums, fresh)),
                        [a, b] => {
                            widen(vnni512_tile(rows, &[a, b], in_size, len, used, sums, fresh))
                        }
                        [a, b, c] => widen(vnni512_tile(
                            rows,
                            &[a, b, c],
                            in_size,
                            len,
                            used,
                            sums,
                            fresh,
                        )),
                        [a, b, c, d] => widen(vnni512_tile(
                            rows,
                            &[a, b, c, d],
                            in_size,
                            len,
                            used,
                            sums,
                            fresh,
                        )),
                        _ => unreachable!("a token tile holds one to four tokens"),
                    }
                },
            );
            kernel
        }
        BatchedKernel::AvxVnni => {
            offset_digits(job, limbs);
            run_vnni::<2, _>(
                job,
                limbs,
                32,
                out,
                |rows, tokens, len, sums, fresh| unsafe {
                    match *tokens {
                        [a] => widen(vnni256_tile(rows, &[a], in_size, len, used, sums, fresh)),
                        [a, b] => {
                            widen(vnni256_tile(rows, &[a, b], in_size, len, used, sums, fresh))
                        }
                        [a, b, c] => widen(vnni256_tile(
                            rows,
                            &[a, b, c],
                            in_size,
                            len,
                            used,
                            sums,
                            fresh,
                        )),
                        [a, b, c, d] => widen(vnni256_tile(
                            rows,
                            &[a, b, c, d],
                            in_size,
                            len,
                            used,
                            sums,
                            fresh,
                        )),
                        _ => unreachable!("a token tile holds one to four tokens"),
                    }
                },
            );
            kernel
        }
        _ => {
            run_quads(job, limbs, out);
            BatchedKernel::Avx2
        }
    }
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const ONE_ROW_KERNEL_UP_TO: usize = 2;

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn run_quads(job: BatchJob<'_>, limbs: &[i8], out: SendPtr) {
    let BatchJob {
        data,
        scales,
        n_rows,
        n_tokens,
        in_size,
        used,
        ..
    } = job;
    let row_block = (131_072 / in_size).clamp(1, n_rows.max(1));
    let n_blocks = n_rows.div_ceil(row_block);
    (0..n_blocks).into_par_iter().for_each(|b| {
        let r0 = b * row_block;
        let r1 = (r0 + row_block).min(n_rows);
        let mut t = 0usize;
        while t < n_tokens {
            let quad = (n_tokens - t).min(4);
            if quad <= ONE_ROW_KERNEL_UP_TO {
                for (offset, &sc) in scales[r0..r1].iter().enumerate() {
                    let i = r0 + offset;
                    for q in 0..quad {
                        let planes =
                            &limbs[(t + q) * LIMB_COUNT * in_size..][..LIMB_COUNT * in_size];
                        let acc = unsafe {
                            dot_limbs_dotprod(data.as_ptr().add(i * in_size), planes, in_size, used)
                        };
                        unsafe { *out.get().add((t + q) * n_rows + i) = (acc * sc) >> FRAC_BITS };
                    }
                }
                t += quad;
                continue;
            }
            let plane_of = |q: usize| {
                let tok = t + q.min(quad - 1);
                unsafe { limbs.as_ptr().add(tok * LIMB_COUNT * in_size) }
            };
            let planes = [plane_of(0), plane_of(1), plane_of(2), plane_of(3)];
            for (offset, &sc) in scales[r0..r1].iter().enumerate() {
                let i = r0 + offset;
                let acc =
                    unsafe { dot_limbs_x4(data.as_ptr().add(i * in_size), &planes, in_size, used) };
                for (q, a) in acc.iter().enumerate().take(quad) {
                    unsafe { *out.get().add((t + q) * n_rows + i) = (*a * sc) >> FRAC_BITS };
                }
            }
            t += quad;
        }
    });
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const TOKEN_BLOCK_BYTES: usize = 512 * 1024;

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn token_block(bytes_per_token: usize, tile: usize) -> usize {
    (TOKEN_BLOCK_BYTES / bytes_per_token.max(1)).max(tile) / tile * tile
}

#[cfg(target_arch = "aarch64")]
fn run_i8mm(job: BatchJob<'_>, limbs: &[i8], out: SendPtr) {
    let BatchJob {
        data,
        scales,
        inputs,
        n_rows,
        n_tokens,
        in_size,
        used,
    } = job;
    let len = in_size / 8 * 8;
    let n_pairs = n_tokens / 2;
    let pair_len = used * 2 * len;
    let mut pairs = vec![0i8; n_pairs * pair_len];
    if pair_len > 0 {
        pairs
            .par_chunks_mut(pair_len)
            .enumerate()
            .for_each(|(pair, interleaved)| {
                for (half, token) in [2 * pair, 2 * pair + 1].into_iter().enumerate() {
                    for (l, plane) in interleaved.chunks_exact_mut(2 * len).enumerate() {
                        let digits = &limbs[(token * LIMB_COUNT + l) * in_size..][..len];
                        for (block, eight) in plane.chunks_exact_mut(16).zip(digits.chunks_exact(8))
                        {
                            block[8 * half..8 * half + 8].copy_from_slice(eight);
                        }
                    }
                }
            });
    }
    let solo_token = (n_tokens % 2 == 1).then_some(n_tokens - 1);
    let solo_pairs = if solo_token.is_some() {
        used.div_ceil(2)
    } else {
        0
    };
    let mut solo = vec![0i8; solo_pairs * 2 * len];
    if let Some(token) = solo_token
        && len > 0
    {
        for (pair, interleaved) in solo.chunks_exact_mut(2 * len).enumerate() {
            for (half, l) in [2 * pair, 2 * pair + 1].into_iter().enumerate() {
                if l >= used {
                    continue;
                }
                let digits = &limbs[(token * LIMB_COUNT + l) * in_size..][..len];
                for (block, eight) in interleaved.chunks_exact_mut(16).zip(digits.chunks_exact(8)) {
                    block[8 * half..8 * half + 8].copy_from_slice(eight);
                }
            }
        }
    }
    let (pairs, solo) = (&pairs[..], &solo[..]);
    let block_pairs = token_block(pair_len, 4);
    let row_block = (131_072 / in_size).max(1).next_multiple_of(4);
    let n_blocks = n_rows.div_ceil(row_block);
    (0..n_blocks).into_par_iter().for_each(|block| {
        let first = block * row_block;
        let end = (first + row_block).min(n_rows);
        let mut p0 = 0usize;
        while p0 < n_pairs {
            let p1 = (p0 + block_pairs).min(n_pairs);
            let mut r = first;
            while r < end {
                let real = (end - r).min(4);
                let row = |k: usize| data[(r + k.min(real - 1)) * in_size..].as_ptr();
                let rows = [[row(0), row(1)], [row(2), row(3)]];
                let mut p = p0;
                while p < p1 {
                    let count = (p1 - p).min(4);
                    let pair = |q: usize| pairs[(p + q.min(count - 1)) * pair_len..].as_ptr();
                    let totals = unsafe {
                        match count {
                            1 => widen(i8mm_tile(&rows, &[pair(0)], len, used)),
                            2 => widen(i8mm_tile(&rows, &[pair(0), pair(1)], len, used)),
                            3 => widen(i8mm_tile(&rows, &[pair(0), pair(1), pair(2)], len, used)),
                            _ => widen(i8mm_tile(
                                &rows,
                                &[pair(0), pair(1), pair(2), pair(3)],
                                len,
                                used,
                            )),
                        }
                    };
                    for (rp, by_pair) in totals.iter().enumerate() {
                        for (q, lanes) in by_pair.iter().enumerate().take(count) {
                            for (lane, &dot) in lanes.iter().enumerate() {
                                let i = r + 2 * rp + lane / 2;
                                let token = 2 * (p + q) + lane % 2;
                                if i >= r + real {
                                    continue;
                                }
                                let acc = dot
                                    + tail_dot(
                                        &data[i * in_size + len..(i + 1) * in_size],
                                        &inputs[token * in_size + len..(token + 1) * in_size],
                                    );
                                unsafe {
                                    *out.get().add(token * n_rows + i) =
                                        (acc * scales[i]) >> FRAC_BITS
                                };
                            }
                        }
                    }
                    p += count;
                }
                r += real;
            }
            p0 = p1;
        }
        let Some(token) = solo_token else {
            return;
        };
        let mut r = first;
        while r < end {
            let real = (end - r).min(4);
            let row = |k: usize| data[(r + k.min(real - 1)) * in_size..].as_ptr();
            let rows = [[row(0), row(1)], [row(2), row(3)]];
            let pair = |q: usize| solo[q.min(solo_pairs - 1) * 2 * len..].as_ptr();
            let totals = unsafe {
                if solo_pairs == 1 {
                    widen(i8mm_tile(&rows, &[pair(0)], len, 1))
                } else {
                    widen(i8mm_tile(&rows, &[pair(0), pair(1)], len, 1))
                }
            };
            for (rp, by_pair) in totals.iter().enumerate() {
                for half in 0..2 {
                    let i = r + 2 * rp + half;
                    if i >= r + real {
                        continue;
                    }
                    let mut acc = tail_dot(
                        &data[i * in_size + len..(i + 1) * in_size],
                        &inputs[token * in_size + len..(token + 1) * in_size],
                    );
                    for (q, lanes) in by_pair.iter().enumerate().take(solo_pairs) {
                        acc += lanes[2 * half] * (1i64 << (16 * q));
                        acc += lanes[2 * half + 1] * (1i64 << (16 * q + 8));
                    }
                    unsafe { *out.get().add(token * n_rows + i) = (acc * scales[i]) >> FRAC_BITS };
                }
            }
            r += real;
        }
    });
}

#[cfg(target_arch = "x86_64")]
fn offset_digits(job: BatchJob<'_>, limbs: &mut [i8]) {
    let span = job.used * job.in_size;
    limbs
        .par_chunks_mut(LIMB_COUNT * job.in_size)
        .for_each(|planes| {
            for digit in &mut planes[..span] {
                *digit ^= i8::MIN;
            }
        });
}

#[cfg(target_arch = "x86_64")]
fn run_vnni<const R: usize, F>(job: BatchJob<'_>, planes: &[i8], step: usize, out: SendPtr, tile: F)
where
    F: Fn(&[*const i8; R], &[*const i8], usize, &mut [i64; R], bool) -> [[i64; 4]; R] + Sync,
{
    let BatchJob {
        data,
        scales,
        inputs,
        n_rows,
        n_tokens,
        in_size,
        used,
    } = job;
    let len = in_size / step * step;
    let block_tokens = token_block(used * in_size, 4);
    let row_block = (131_072 / in_size).max(1).next_multiple_of(R);
    let n_blocks = n_rows.div_ceil(row_block);
    (0..n_blocks).into_par_iter().for_each(|block| {
        let first = block * row_block;
        let end = (first + row_block).min(n_rows);
        let mut t0 = 0usize;
        while t0 < n_tokens {
            let t1 = (t0 + block_tokens).min(n_tokens);
            let mut r = first;
            while r < end {
                let real = (end - r).min(R);
                let row_of = |k: usize| r + k.min(real - 1);
                let rows: [*const i8; R] =
                    std::array::from_fn(|k| data[row_of(k) * in_size..].as_ptr());
                let mut sums = [0i64; R];
                let mut t = t0;
                while t < t1 {
                    let count = (t1 - t).min(4);
                    let tokens: [*const i8; 4] = std::array::from_fn(|q| {
                        planes[(t + q.min(count - 1)) * LIMB_COUNT * in_size..].as_ptr()
                    });
                    let totals = tile(&rows, &tokens[..count], len, &mut sums, t == t0);
                    for (k, by_token) in totals.iter().enumerate().take(real) {
                        let i = r + k;
                        for (q, &dot) in by_token.iter().enumerate().take(count) {
                            let token = t + q;
                            let acc = dot
                                + tail_dot(
                                    &data[i * in_size + len..(i + 1) * in_size],
                                    &inputs[token * in_size + len..(token + 1) * in_size],
                                );
                            unsafe {
                                *out.get().add(token * n_rows + i) = (acc * scales[i]) >> FRAC_BITS
                            };
                        }
                    }
                    t += count;
                }
                r += real;
            }
            t0 = t1;
        }
    });
}

#[cfg(test)]
pub(crate) static KERNEL_SWITCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn kernel_switch_guard() -> std::sync::MutexGuard<'static, ()> {
    KERNEL_SWITCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integer_lut::FRAC_BITS;

    fn census_guard() -> std::sync::MutexGuard<'static, ()> {
        kernel_switch_guard()
    }

    fn scalar_dot(row: &[i8], input: &[i64]) -> i64 {
        row.iter()
            .zip(input)
            .fold(0i64, |a, (w, x)| a + (*w as i64) * *x)
    }

    fn weights_from(rows: usize, cols: usize, mut f: impl FnMut(usize, usize) -> i8) -> I8Weights {
        let mut data = Vec::with_capacity(rows * cols);
        for r in 0..rows {
            for c in 0..cols {
                data.push(f(r, c));
            }
        }
        I8Weights {
            data,
            scales: (0..rows).map(|i| (i as i64 % 97) + 1).collect(),
            n_rows: rows,
            n_cols: cols,
        }
    }

    #[test]
    fn limb_bounds_are_derived_not_assumed() {
        assert!(limb_bounds_match_formula());
        assert!(LIMB_MAX < i32::MAX as i64);
        assert!(LIMB_MIN < -(i32::MAX as i64));
    }

    #[test]
    fn split_is_exact_or_refuses() {
        let edges: Vec<i64> = vec![
            0,
            1,
            -1,
            127,
            128,
            -128,
            -129,
            255,
            256,
            -256,
            32767,
            32768,
            -32768,
            65535,
            65536,
            16_777_215,
            16_777_216,
            LIMB_MAX,
            LIMB_MIN,
            LIMB_MAX - 1,
            LIMB_MIN + 1,
        ];
        let mut limbs = vec![0i8; LIMB_COUNT * edges.len()];
        let used = split_limbs(&edges, &mut limbs).expect("edge values are in domain");
        assert!(used <= LIMB_COUNT);
        for (j, &x) in edges.iter().enumerate() {
            let mut r = 0i64;
            for i in (0..LIMB_COUNT).rev() {
                r = r * 256 + limbs[i * edges.len() + j] as i64;
            }
            assert_eq!(r, x, "digit reconstruction must be exact for {x}");
        }
        for bad in [
            LIMB_MAX + 1,
            LIMB_MIN - 1,
            i32::MAX as i64,
            i64::MAX,
            i64::MIN,
        ] {
            let v = vec![1i64, bad, 3];
            let mut l = vec![0i8; LIMB_COUNT * v.len()];
            assert!(split_limbs(&v, &mut l).is_none(), "{bad} must be refused");
        }
    }

    #[test]
    fn matches_scalar_on_model_shapes_and_tails() {
        if !dotprod_available() {
            eprintln!("dotprod unavailable on this target; vectorised path not exercised");
            return;
        }
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for &cols in &[
            1usize, 7, 15, 16, 17, 31, 32, 33, 63, 65, 255, 4096, 4097, 11008,
        ] {
            for rows in [1usize, 3, 256, 257] {
                let w = weights_from(rows, cols, |_, _| (next() % 256) as u8 as i8);
                let mut w = w;
                if !w.data.is_empty() {
                    w.data[0] = -128;
                    *w.data.last_mut().unwrap() = -128;
                }
                let input: Vec<i64> = (0..cols)
                    .map(|j| match j % 5 {
                        0 => LIMB_MAX,
                        1 => LIMB_MIN,
                        2 => 0,
                        3 => -(((next() % 8_388_608) as i64) + 1),
                        _ => ((next() % 8_388_608) as i64) + 1,
                    })
                    .collect();
                let mut fast = vec![0i64; rows];
                assert!(
                    matmul_i8_canonical_rows_fast(&w, &input, cols, &mut fast),
                    "must accept cols={cols} rows={rows}"
                );
                for (i, got) in fast.iter().enumerate() {
                    let acc = scalar_dot(&w.data[i * cols..(i + 1) * cols], &input);
                    let want = (acc * w.scales[i]) >> FRAC_BITS;
                    assert_eq!(*got, want, "cols={cols} rows={rows} row={i}");
                }
            }
        }
    }

    #[test]
    fn refuses_inner_dimension_above_the_i32_bound() {
        let cols = MAX_COLS_FOR_I32 + 1;
        assert_eq!(cols, crate::tensor_parallel::MAX_ROW_INPUT_ELEMENTS);
        let w = weights_from(1, cols, |_, _| -128);
        let input = vec![LIMB_MAX; cols];
        let mut out = vec![0i64; 1];
        assert!(
            !matmul_i8_canonical_rows_fast(&w, &input, cols, &mut out),
            "K one over the derived i32 bound must be refused"
        );
        assert_eq!(out[0], 0, "a refused call must not write output");
    }

    #[test]
    fn exact_vector_backend_handles_the_maximum_inner_dimension() {
        let _guard = kernel_switch_guard();
        if !dotprod_available() {
            return;
        }
        let cols = MAX_COLS_FOR_I32;
        let w = weights_from(1, cols, |_, _| -128);
        let input = vec![LIMB_MIN; cols];
        let mut output = vec![0; 1];
        assert!(matmul_i8_canonical_rows_fast(&w, &input, cols, &mut output));
        assert_eq!(output[0], scalar_dot(&w.data, &input) >> FRAC_BITS);
    }

    #[test]
    fn exact_batched_backend_matches_scalar_across_short_quads_and_tails() {
        let _guard = kernel_switch_guard();
        if !dotprod_available() {
            return;
        }
        for cols in [1, 15, 16, 17, 33, 4096] {
            let rows = 5;
            let w = weights_from(rows, cols, |r, c| (r * 73 + c * 19) as u8 as i8);
            for tokens in [1, 2, 3, 4, 5, 7] {
                let inputs: Vec<i64> = (0..tokens * cols)
                    .map(|i| match (i / cols + i % cols) % 5 {
                        0 => LIMB_MIN,
                        1 => LIMB_MAX,
                        2 => -129,
                        3 => 1,
                        _ => 65536,
                    })
                    .collect();
                let mut output = vec![0; tokens * rows];
                assert!(matmul_i8_batched_fast(
                    &w,
                    &inputs,
                    tokens,
                    cols,
                    &mut output
                ));
                for token in 0..tokens {
                    for row in 0..rows {
                        let dot = scalar_dot(
                            &w.data[row * cols..(row + 1) * cols],
                            &inputs[token * cols..(token + 1) * cols],
                        );
                        assert_eq!(
                            output[token * rows + row],
                            (dot * w.scales[row]) >> FRAC_BITS,
                            "cols={cols}, tokens={tokens}, token={token}, row={row}"
                        );
                    }
                }
            }
        }
    }

    fn weights_with_scale(rows: usize, cols: usize, scale: i64) -> I8Weights {
        I8Weights {
            data: vec![-128i8; rows * cols],
            scales: vec![scale; rows],
            n_rows: rows,
            n_cols: cols,
        }
    }

    const SENTINEL: i64 = 0x7E57_7E57_7E57_7E57;

    fn assert_untouched(out: &[i64], case: &str) {
        assert!(
            out.iter().all(|v| *v == SENTINEL),
            "refusal must not write output ({case})"
        );
    }

    #[test]
    fn post_scale_bound_is_checked_at_this_entry_point() {
        if !dotprod_available() {
            return;
        }
        let cols = 32usize;
        let input = vec![1i64; cols];
        let dot_bound = 128i64 * cols as i64;
        assert_eq!(dot_bound, 4096);

        let ok_scale = i64::MAX / dot_bound;
        assert!(dot_bound.checked_mul(ok_scale).is_some());
        let w = weights_with_scale(4, cols, ok_scale);
        let mut out = vec![SENTINEL; 4];
        assert!(
            matmul_i8_canonical_rows_fast(&w, &input, cols, &mut out),
            "the scale boundary case must be accepted, not refused"
        );
        let acc = scalar_dot(&w.data[..cols], &input);
        assert_eq!(out[0], (acc * ok_scale) >> FRAC_BITS);

        let bad_scale = ok_scale + 1;
        assert!(dot_bound.checked_mul(bad_scale).is_none());
        let w = weights_with_scale(4, cols, bad_scale);
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(&w, &input, cols, &mut out));
        assert_untouched(&out, "scale one over the bound");

        let w = weights_with_scale(4, cols, i64::MIN);
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(&w, &input, cols, &mut out));
        assert_untouched(&out, "i64::MIN scale");
    }

    #[test]
    fn every_refusal_leaves_output_untouched() {
        let cols = 64usize;
        let good = vec![7i64; cols];

        let mut w = weights_with_scale(4, cols, 1);
        w.scales.pop();
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(&w, &good, cols, &mut out));
        assert_untouched(&out, "shape");

        let w = weights_with_scale(4, cols, 1);
        let mut bad_input = good.clone();
        bad_input[cols / 2] = LIMB_MAX + 1;
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(
            &w, &bad_input, cols, &mut out
        ));
        assert_untouched(&out, "activation out of domain");

        let big = MAX_COLS_FOR_I32 + 1;
        let w = weights_with_scale(1, big, 1);
        let input = vec![1i64; big];
        let mut out = vec![SENTINEL; 1];
        assert!(!matmul_i8_canonical_rows_fast(&w, &input, big, &mut out));
        assert_untouched(&out, "inner dim above i32 bound");

        let w = weights_with_scale(4, cols, i64::MAX);
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(&w, &good, cols, &mut out));
        assert_untouched(&out, "post-scale overflow");
    }

    #[test]
    fn census_records_attempts_accepts_and_refusal_reasons() {
        if !dotprod_available() {
            return;
        }
        let _guard = census_guard();
        let cols = 64usize;
        let good = vec![7i64; cols];
        reset_projection_census();
        set_projection_census_enabled(true);

        let w = weights_with_scale(4, cols, 1);
        let mut out = vec![0i64; 4];
        assert!(matmul_i8_canonical_rows_fast(&w, &good, cols, &mut out));

        let mut bad_input = good.clone();
        bad_input[0] = LIMB_MIN - 1;
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(
            &w, &bad_input, cols, &mut out
        ));

        let w_bad = weights_with_scale(4, cols, i64::MAX);
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(
            &w_bad, &good, cols, &mut out
        ));

        let c = projection_census();
        set_projection_census_enabled(false);
        assert!(c.attempted >= 3, "{c:?}");
        assert!(c.accepted >= 1, "{c:?}");
        assert!(c.refused_activation_out_of_domain >= 1, "{c:?}");
        assert!(c.refused_scale_multiply_would_overflow >= 1, "{c:?}");
        reset_projection_census();
        assert!(
            !CENSUS_ON.load(Ordering::Relaxed),
            "the census test must leave counting disabled"
        );
    }

    #[test]
    fn census_is_off_by_default_and_costs_nothing_when_off() {
        let _guard = census_guard();
        assert!(!CENSUS_ON.load(Ordering::Relaxed));
    }

    fn choice(requested: Option<&str>, legacy: Option<&str>, default: bool) -> (bool, u8, bool) {
        let resolved = choose_kernel(requested, legacy, default);
        (resolved.simd, resolved.source, resolved.unrecognised)
    }

    #[test]
    fn kernel_overrides_take_precedence_and_fail_safe_to_scalar() {
        assert_eq!(choice(None, None, true), (true, SOURCE_DEFAULT, false));
        assert_eq!(choice(None, None, false), (false, SOURCE_DEFAULT, false));
        for value in ["scalar", "SCALAR", " Scalar "] {
            let forced = choice(Some(value), None, true);
            assert_eq!(forced, (false, SOURCE_ENV, false), "{value:?}");
        }
        assert_eq!(choice(Some("simd"), None, false), (true, SOURCE_ENV, false));
        assert_eq!(choice(Some("auto"), None, true), (true, SOURCE_ENV, false));
        assert_eq!(
            choice(Some("auto"), None, false),
            (false, SOURCE_ENV, false)
        );
        let new_wins = choice(Some("scalar"), Some("1"), true);
        assert_eq!(new_wins, (false, SOURCE_ENV, false));
        assert_eq!(
            choice(Some("simd"), Some("0"), false),
            (true, SOURCE_ENV, false)
        );
        let typo = choice(Some("scaler"), None, true);
        assert_eq!(typo, (false, SOURCE_ENV, true));
        let empty = choice(Some(" "), None, true);
        assert_eq!(empty, (true, SOURCE_DEFAULT, false));
        let legacy_on = choice(None, Some("1"), false);
        assert_eq!(legacy_on, (true, SOURCE_LEGACY_ENV, false));
        for value in ["0", "", "off", "true"] {
            let legacy_off = choice(None, Some(value), true);
            assert_eq!(legacy_off, (false, SOURCE_LEGACY_ENV, false), "{value:?}");
        }
    }

    #[test]
    fn the_default_is_avx2_on_x86_and_follows_the_neon_switch_on_arm64() {
        #[cfg(target_arch = "x86_64")]
        assert_eq!(simd_on_by_default(), !running_under_rosetta());
        #[cfg(target_arch = "aarch64")]
        assert_eq!(simd_on_by_default(), NEON_ON_BY_DEFAULT);
    }

    #[test]
    fn an_explicit_choice_wins_and_is_reported() {
        let _guard = kernel_switch_guard();
        let previous = fast_canonical_kernel_enabled();

        set_fast_canonical_kernel(false);
        assert!(!fast_canonical_kernel_enabled());
        assert_eq!(effective_kernel_name(), "scalar-i8xi64");
        assert_eq!(kernel_choice_source(), "explicit");

        set_fast_canonical_kernel(true);
        assert_eq!(fast_canonical_kernel_enabled(), dotprod_available());
        let expected = if !dotprod_available() {
            "scalar-i8xi64"
        } else if cfg!(target_arch = "x86_64") {
            "avx2-limb"
        } else {
            "neon-sdot-limb"
        };
        assert_eq!(effective_kernel_name(), expected);
        assert_eq!(kernel_choice_source(), "explicit");

        set_fast_canonical_kernel(previous);
    }

    #[test]
    fn the_thread_census_counts_only_this_threads_projections() {
        let _ = take_thread_census();
        let cols = 64usize;
        let w = weights_with_scale(4, cols, 1);
        let good = vec![7i64; cols];
        let mut out = vec![0i64; 4];
        let accepted = matmul_i8_canonical_rows_fast(&w, &good, cols, &mut out);
        let mut bad = good.clone();
        bad[0] = LIMB_MAX + 1;
        assert!(!matmul_i8_canonical_rows_fast(&w, &bad, cols, &mut out));
        let [attempted, accepted_count, refused] = take_thread_census();
        assert_eq!(attempted, 2);
        assert_eq!(accepted_count, u64::from(accepted));
        assert_eq!(refused, 2 - u64::from(accepted));
        assert_eq!(accepted, dotprod_available());
        assert_eq!(take_thread_census(), [0; 3], "taking the census resets it");
    }

    fn scalar_batched(w: &I8Weights, inputs: &[i64], tokens: usize, cols: usize) -> Vec<i64> {
        let mut out = vec![0i64; tokens * w.n_rows];
        for t in 0..tokens {
            for r in 0..w.n_rows {
                let dot = scalar_dot(
                    &w.data[r * cols..(r + 1) * cols],
                    &inputs[t * cols..(t + 1) * cols],
                );
                out[t * w.n_rows + r] = (dot * w.scales[r]) >> FRAC_BITS;
            }
        }
        out
    }

    #[test]
    fn batched_kernel_report() {
        let _guard = kernel_switch_guard();
        let available: Vec<&str> = available_batched_kernels()
            .into_iter()
            .map(BatchedKernel::label)
            .collect();
        println!("cpu features detected: {:?}", detected_cpu_features());
        println!("batched kernels available: {available:?}");
        println!(
            "batched kernel selected automatically: {:?}",
            best_batched_kernel().map(BatchedKernel::label)
        );
        println!(
            "fast kernel enabled: {}; kernel selected now: {:?}",
            fast_canonical_kernel_enabled(),
            selected_batched_kernel().map(BatchedKernel::label)
        );
        if let Ok(path) = std::env::var("ARC_BATCHED_KERNELS_OUT") {
            std::fs::write(&path, available.join("\n")).expect("write the kernel list");
        }
        if let Ok(expected) = std::env::var("ARC_EXPECT_BATCHED_KERNELS") {
            for label in expected
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|label| !label.is_empty())
            {
                let kernel = BatchedKernel::from_label(label)
                    .unwrap_or_else(|| panic!("unknown kernel {label}"));
                assert!(
                    kernel.available(),
                    "the OS reports the CPU flags for {label}, but it was not detected"
                );
            }
        }
        let report = kernel_report();
        println!("kernel report: {report}");
        if let Some(pinned) = report.pin_env.and_then(BatchedKernel::from_label) {
            assert_eq!(report.pin, Some(pinned), "{report}");
            if report.vector_enabled && pinned.available() {
                assert_eq!(report.pin_status, PinStatus::Honoured, "{report}");
                assert_eq!(report.one_token, Some(pinned), "{report}");
                assert_eq!(report.batched, Some(pinned), "{report}");
            }
        }
    }

    #[test]
    fn every_batched_kernel_matches_scalar_on_every_tile_shape() {
        let kernels = available_batched_kernels();
        if kernels.is_empty() {
            eprintln!("no batched kernel on this target; nothing to compare");
            return;
        }
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for cols in [
            1usize, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 4096, 4097,
        ] {
            for rows in [1usize, 2, 3, 4, 5, 9] {
                let mut w = weights_from(rows, cols, |_, _| (next() % 256) as u8 as i8);
                w.data[0] = -128;
                *w.data.last_mut().unwrap() = 127;
                for tokens in [1usize, 2, 3, 5, 8, 9, 17, 64] {
                    let inputs: Vec<i64> = (0..tokens * cols)
                        .map(|i| match (next() % 7, i % 3) {
                            (0, _) => LIMB_MIN,
                            (1, _) => LIMB_MAX,
                            (2, _) => 0,
                            (3, 0) => -129,
                            (3, _) => 65_536,
                            (4, _) => -(((next() % 8_388_608) as i64) + 1),
                            _ => ((next() % 8_388_608) as i64) + 1,
                        })
                        .collect();
                    let want = scalar_batched(&w, &inputs, tokens, cols);
                    for &kernel in &kernels {
                        let mut got = vec![SENTINEL; tokens * rows];
                        assert!(
                            matmul_i8_batched_with(kernel, &w, &inputs, tokens, cols, &mut got),
                            "{} refused cols={cols} rows={rows} tokens={tokens}",
                            kernel.label()
                        );
                        assert_eq!(
                            got,
                            want,
                            "{} cols={cols} rows={rows} tokens={tokens}",
                            kernel.label()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn every_batched_kernel_is_exact_at_the_i32_lane_bound() {
        let kernels = available_batched_kernels();
        if kernels.is_empty() {
            return;
        }
        let cols = MAX_COLS_FOR_I32;
        let rows = 5;
        let w = weights_from(rows, cols, |r, c| match r {
            0 | 1 => -128,
            2 => 127,
            _ if c % 2 == 0 => -128,
            _ => 127,
        });
        let inputs: Vec<i64> = (0..3 * cols)
            .map(|i| match (i / cols, i % 2) {
                (0, _) => LIMB_MIN,
                (1, _) => LIMB_MAX,
                (_, 0) => LIMB_MAX,
                _ => LIMB_MIN,
            })
            .collect();
        for tokens in [1, 3] {
            let inputs = &inputs[..tokens * cols];
            let want = scalar_batched(&w, inputs, tokens, cols);
            for &kernel in &kernels {
                let mut got = vec![SENTINEL; tokens * rows];
                assert!(
                    matmul_i8_batched_with(kernel, &w, inputs, tokens, cols, &mut got),
                    "{} refused K = {cols}, {tokens} tokens",
                    kernel.label()
                );
                assert_eq!(
                    got,
                    want,
                    "{} at K = {cols}, {tokens} tokens",
                    kernel.label()
                );
            }
        }
    }

    #[test]
    fn batched_kernel_selection_falls_back_to_the_best_available() {
        let _guard = kernel_switch_guard();
        let previous = batched_kernel_preference();
        let mut labels: Vec<&str> = BatchedKernel::ALL.map(BatchedKernel::label).to_vec();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), BatchedKernel::ALL.len(), "labels are unique");
        for kernel in BatchedKernel::ALL {
            assert_eq!(BatchedKernel::from_label(kernel.label()), Some(kernel));
        }
        assert_eq!(BatchedKernel::from_label("auto"), None);
        let best = best_batched_kernel();
        assert_eq!(best.is_some(), dotprod_available());
        for kernel in BatchedKernel::ALL {
            set_batched_kernel_preference(Some(kernel));
            let want = if kernel.available() {
                Some(kernel)
            } else {
                best
            };
            assert_eq!(selected_batched_kernel(), want, "{}", kernel.label());
        }
        set_batched_kernel_preference(None);
        assert_eq!(selected_batched_kernel(), best);
        set_batched_kernel_preference(previous);
    }

    #[test]
    fn a_kernel_this_cpu_lacks_is_refused_without_writing() {
        let Some(missing) = BatchedKernel::ALL.into_iter().find(|k| !k.available()) else {
            return;
        };
        let cols = 64usize;
        let w = weights_with_scale(4, cols, 1);
        let inputs = vec![7i64; 2 * cols];
        let mut out = vec![SENTINEL; 2 * 4];
        assert!(!matmul_i8_batched_with(
            missing, &w, &inputs, 2, cols, &mut out
        ));
        assert_untouched(&out, missing.label());
    }

    #[test]
    fn the_kernel_that_ran_is_recorded() {
        let cols = 96usize;
        let w = weights_from(8, cols, |r, c| (r * 31 + c * 7) as u8 as i8);
        let inputs: Vec<i64> = (0..3 * cols as i64).map(|i| i * 1_000 - 50_000).collect();
        for kernel in available_batched_kernels() {
            let before = batched_kernel_runs(kernel);
            let mut out = vec![0i64; 3 * 8];
            assert!(matmul_i8_batched_with(
                kernel, &w, &inputs, 3, cols, &mut out
            ));
            assert!(
                batched_kernel_runs(kernel) > before,
                "{} ran but was not recorded",
                kernel.label()
            );
        }
        if BatchedKernel::NeonI8mm.available() {
            let before = batched_kernel_runs(BatchedKernel::NeonI8mm);
            let mut out = vec![0i64; 8];
            assert!(matmul_i8_batched_with(
                BatchedKernel::NeonI8mm,
                &w,
                &inputs[..cols],
                1,
                cols,
                &mut out
            ));
            assert!(batched_kernel_runs(BatchedKernel::NeonI8mm) > before);
        }
    }

    #[test]
    fn one_row_entry_matches_scalar_on_every_kernel() {
        let _guard = kernel_switch_guard();
        let previous = batched_kernel_preference();
        let mut seed = 0x1234_5678_9ABC_DEF1u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for kernel in available_batched_kernels() {
            set_batched_kernel_preference(Some(kernel));
            assert_eq!(selected_batched_kernel(), Some(kernel));
            for cols in [1usize, 8, 9, 31, 64, 65, 4096, 4097] {
                for rows in [1usize, 3, 4, 9] {
                    let w = weights_from(rows, cols, |_, _| (next() % 256) as u8 as i8);
                    let input: Vec<i64> = (0..cols)
                        .map(|j| match (next() % 5, j % 2) {
                            (0, _) => LIMB_MIN,
                            (1, _) => LIMB_MAX,
                            (2, _) => 0,
                            (_, 0) => -(((next() % 8_388_608) as i64) + 1),
                            _ => ((next() % 8_388_608) as i64) + 1,
                        })
                        .collect();
                    let want = scalar_batched(&w, &input, 1, cols);
                    let before = batched_kernel_runs(kernel);
                    let mut got = vec![SENTINEL; rows];
                    assert!(
                        matmul_i8_canonical_rows_fast(&w, &input, cols, &mut got),
                        "{} refused one row, cols={cols} rows={rows}",
                        kernel.label()
                    );
                    assert_eq!(got, want, "{} cols={cols} rows={rows}", kernel.label());
                    if kernel.serves_one_row() {
                        assert!(
                            batched_kernel_runs(kernel) > before,
                            "{} served one row but was not recorded",
                            kernel.label()
                        );
                    }
                }
            }
        }
        set_batched_kernel_preference(previous);
    }

    fn the_kernel_that_ran(runs: [u64; 5], case: &str) -> Option<BatchedKernel> {
        let ran: Vec<BatchedKernel> = BatchedKernel::ALL
            .into_iter()
            .filter(|kernel| runs[*kernel as usize] > 0)
            .collect();
        assert!(ran.len() <= 1, "{case}: more than one kernel ran: {runs:?}");
        ran.first().copied()
    }

    #[test]
    fn kernel_report_names_the_kernels_the_counters_show_ran() {
        let _guard = kernel_switch_guard();
        let previous_vector = fast_canonical_kernel_enabled();
        let previous_pin = batched_kernel_preference();
        let (rows, cols, tokens) = (9usize, 200usize, 3usize);
        let w = weights_from(rows, cols, |r, c| (r * 37 + c * 11) as u8 as i8);
        let inputs: Vec<i64> = (0..(tokens * cols) as i64)
            .map(|i| i * 7_919 - 600_000)
            .collect();
        let want = scalar_batched(&w, &inputs, tokens, cols);
        let pins: Vec<Option<BatchedKernel>> = std::iter::once(None)
            .chain(BatchedKernel::ALL.map(Some))
            .collect();
        for vector in [true, false] {
            for &pin in &pins {
                set_fast_canonical_kernel(vector);
                set_batched_kernel_preference(pin);
                let report = kernel_report();
                let case = format!(
                    "vector kernels {vector}, pin {:?}: {report}",
                    pin.map(BatchedKernel::label)
                );
                let on = vector && dotprod_available();
                let expected = if on {
                    pin.filter(|kernel| kernel.available())
                        .or_else(best_batched_kernel)
                } else {
                    None
                };
                assert_eq!(report.vector_enabled, on, "{case}");
                assert_eq!(report.one_token, expected, "{case}");
                assert_eq!(report.batched, expected, "{case}");
                assert_eq!(report.pin, pin, "{case}");
                match pin {
                    None => assert!(
                        matches!(
                            report.pin_status,
                            PinStatus::Unpinned | PinStatus::Unrecognised
                        ),
                        "{case}"
                    ),
                    Some(kernel) => {
                        let status = if !on {
                            PinStatus::Inactive
                        } else if kernel.available() {
                            PinStatus::Honoured
                        } else {
                            PinStatus::FellBack
                        };
                        assert_eq!(report.pin_status, status, "{case}");
                        assert_eq!(
                            report.pin_missing_cpu_features.is_empty(),
                            kernel.available(),
                            "{case}"
                        );
                    }
                }

                let _ = take_thread_kernel_runs();
                let one_row_before = expected.map(one_row_kernel_runs);
                let mut got = vec![SENTINEL; rows];
                crate::cached_integer_model::matmul_i8_into(&w, &inputs[..cols], cols, &mut got);
                assert_eq!(got.as_slice(), &want[..rows], "{case}: one row");
                let [one_row, _] = take_thread_kernel_runs();
                assert_eq!(
                    the_kernel_that_ran(one_row, &case),
                    report.one_token,
                    "{case}: one row"
                );
                if let (Some(kernel), Some(before)) = (expected, one_row_before) {
                    assert!(one_row_kernel_runs(kernel) > before, "{case}: one row");
                }

                let batched_before = expected.map(batched_kernel_runs);
                let mut got = vec![SENTINEL; tokens * rows];
                crate::cached_integer_model::matmul_i8_into_batched(
                    &w, &inputs, tokens, cols, &mut got,
                );
                assert_eq!(got, want, "{case}: {tokens} rows");
                let [_, batched] = take_thread_kernel_runs();
                assert_eq!(
                    the_kernel_that_ran(batched, &case),
                    report.batched,
                    "{case}: {tokens} rows"
                );
                if let (Some(kernel), Some(before)) = (expected, batched_before) {
                    assert!(
                        batched_kernel_runs(kernel) > before,
                        "{case}: {tokens} rows"
                    );
                }
            }
        }
        set_batched_kernel_preference(previous_pin);
        set_fast_canonical_kernel(previous_vector);
    }

    #[test]
    fn a_kernel_is_available_exactly_when_no_feature_it_needs_is_missing() {
        for kernel in BatchedKernel::ALL {
            let missing = kernel.missing_cpu_features();
            assert_eq!(missing.is_empty(), kernel.available(), "{}", kernel.label());
            for feature in &missing {
                assert!(kernel.required_cpu_features().contains(feature));
            }
        }
    }

    #[test]
    fn kernel_report_reads_as_one_line_per_pin_status() {
        let unpinned = KernelReport {
            vector_enabled: true,
            one_token: Some(BatchedKernel::Avx512Vnni),
            batched: Some(BatchedKernel::Avx512Vnni),
            pin: None,
            pin_env: None,
            pin_status: PinStatus::Unpinned,
            pin_missing_cpu_features: Vec::new(),
        };
        let honoured = KernelReport {
            one_token: Some(BatchedKernel::Avx2),
            batched: Some(BatchedKernel::Avx2),
            pin: Some(BatchedKernel::Avx2),
            pin_env: Some("avx2-limb"),
            pin_status: PinStatus::Honoured,
            ..unpinned.clone()
        };
        let fell_back = KernelReport {
            pin: Some(BatchedKernel::Avx512Vnni),
            pin_env: Some("avx512-vnni-limb"),
            pin_status: PinStatus::FellBack,
            pin_missing_cpu_features: vec!["avx512f", "avx512vnni"],
            ..honoured.clone()
        };
        let inactive = KernelReport {
            vector_enabled: false,
            one_token: None,
            batched: None,
            pin_status: PinStatus::Inactive,
            ..honoured.clone()
        };
        let unrecognised = KernelReport {
            pin_env: Some("vnni"),
            pin_status: PinStatus::Unrecognised,
            ..unpinned.clone()
        };
        let lines = [
            (
                unpinned,
                "one-token avx512-vnni-limb, batched avx512-vnni-limb; no pin",
            ),
            (
                honoured,
                "one-token avx2-limb, batched avx2-limb; pin avx2-limb honoured",
            ),
            (
                fell_back,
                "one-token avx2-limb, batched avx2-limb; pin avx512-vnni-limb fell back, \
                 this CPU lacks avx512f, avx512vnni",
            ),
            (
                inactive,
                "one-token scalar-i8xi64, batched scalar-i8xi64; pin avx2-limb inactive, \
                 the vector kernels are off",
            ),
            (
                unrecognised,
                "one-token avx512-vnni-limb, batched avx512-vnni-limb; \
                 ARC_CANONICAL_BATCHED_KERNEL=\"vnni\" names no kernel",
            ),
        ];
        for (report, line) in lines {
            assert_eq!(report.to_string(), line);
        }
    }
}
