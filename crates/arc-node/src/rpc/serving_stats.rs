//! Serving v2: completed public answers on this coordinator, never replicas,
//! verification recomputations or demand-pump work. Windows are [end-length,end)
//! anchored to the last complete UTC minute (monotonic elapsed from UTC origin).
//! Startup counts/rates are null.
//! Percentiles interpolate at (n-1)*p over the newest 1,000 answers per minute;
//! consumers must pool samples, not percentiles. GET never expires state.
//! Only fixed-size model hashes and numeric measurements enter this store.
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DAY: u64 = 86_400;
const WINDOW: u64 = 60;
const SAMPLE_CAP: usize = 1_000;
const MODEL_CAP: usize = 32;
const HOP_CAP: usize = 128;

#[derive(Clone, Default)]
pub(super) struct AnswerTiming {
    pub first: Option<Duration>,
    pub last: Option<Duration>,
}
impl AnswerTiming {
    pub fn token(&mut self, elapsed: Duration) {
        self.first.get_or_insert(elapsed);
        self.last = Some(elapsed);
    }
    fn rate(&self, output: u64) -> Option<f64> {
        let seconds = self.last?.checked_sub(self.first?)?.as_secs_f64();
        (output >= 2 && seconds > 0.0).then(|| (output - 1) as f64 / seconds)
    }
}
#[derive(Clone, Serialize)]
pub(super) struct HopSample {
    pub hop: usize,
    pub start_layer: usize,
    pub end_layer: usize,
    pub positions: u64,
    /// Coordinator round trip including compute; not pure network RTT.
    pub wall_ms: u64,
    pub compute_ms: u64,
}
pub(super) struct Answer {
    pub model: [u8; 32],
    /// Tokenized request including template, excluding synthetic BOS.
    pub input: Option<u64>,
    pub output: u64,
    pub verified: bool,
    pub cached: bool,
    pub timing: AnswerTiming,
    pub hops: Vec<HopSample>,
}
#[derive(Default)]
struct Bucket {
    second: u64,
    input: u64,
    unknown_input: u64,
    output: u64,
    verified: u64,
    answers: u64,
    cached: u64,
    untimed_answers_no_worker_timestamps: u64,
    short_answers: u64,
    zero_output_answers: u64,
    decode_missing_timestamps_answers: u64,
    ttft_missing_timestamps_answers: u64,
    invalid_decode_timing_answers: u64,
    speed_eligible_answers: u64,
    ttft_eligible_answers: u64,
}
struct Sample {
    second: u64,
    rate: Option<f64>,
    ttft_ms: Option<f64>,
    omitted_hops: usize,
    hops: Vec<HopSample>,
}
#[derive(Default)]
struct Model {
    buckets: VecDeque<Bucket>,
    samples: BTreeMap<u64, VecDeque<Sample>>,
}
pub(super) struct ServingStats {
    started: Instant,
    since_unix_ms: u64,
    models: BTreeMap<[u8; 32], Model>,
    rejected_models: u64,
}
impl Default for ServingStats {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            since_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            models: BTreeMap::new(),
            rejected_models: 0,
        }
    }
}
impl ServingStats {
    pub fn record(&mut self, answer: Answer) {
        self.record_at(
            (self.since_unix_ms + self.started.elapsed().as_millis() as u64) / 1000,
            answer,
        );
    }
    fn record_at(&mut self, second: u64, answer: Answer) {
        if !self.models.contains_key(&answer.model) && self.models.len() >= MODEL_CAP {
            self.rejected_models = self.rejected_models.saturating_add(1);
            return;
        }
        let model = self.models.entry(answer.model).or_default();
        while model
            .buckets
            .front()
            .is_some_and(|b| b.second < (second / WINDOW * WINDOW).saturating_sub(DAY))
        {
            model.buckets.pop_front();
        }
        if model.buckets.back().is_none_or(|b| b.second != second) {
            model.buckets.push_back(Bucket {
                second,
                ..Bucket::default()
            });
        }
        let b = model.buckets.back_mut().expect("bucket inserted");
        b.answers += 1;
        b.input += answer.input.unwrap_or(0);
        b.unknown_input += u64::from(answer.input.is_none());
        b.output += answer.output;
        b.verified += if answer.verified { answer.output } else { 0 };
        b.cached += u64::from(answer.cached);
        let rate = (!answer.cached)
            .then(|| answer.timing.rate(answer.output))
            .flatten();
        let ttft_ms = if answer.cached || answer.output == 0 {
            None
        } else {
            answer.timing.first.map(|d| d.as_secs_f64() * 1000.0)
        };
        let missing_decode = !answer.cached
            && answer.output >= 2
            && (answer.timing.first.is_none() || answer.timing.last.is_none());
        let missing_ttft = !answer.cached && answer.output > 0 && answer.timing.first.is_none();
        b.untimed_answers_no_worker_timestamps += u64::from(missing_decode || missing_ttft);
        b.short_answers += u64::from(!answer.cached && answer.output < 2);
        b.zero_output_answers += u64::from(!answer.cached && answer.output == 0);
        b.decode_missing_timestamps_answers += u64::from(missing_decode);
        b.ttft_missing_timestamps_answers += u64::from(missing_ttft);
        b.invalid_decode_timing_answers +=
            u64::from(!answer.cached && answer.output >= 2 && !missing_decode && rate.is_none());
        b.speed_eligible_answers += u64::from(rate.is_some());
        b.ttft_eligible_answers += u64::from(ttft_ms.is_some());
        // Separate caps preserve the completed minute during writes in the current minute.
        let minute = second / WINDOW;
        model.samples.retain(|m, _| *m >= minute.saturating_sub(1));
        let samples = model.samples.entry(minute).or_default();
        if samples.len() == SAMPLE_CAP {
            samples.pop_front();
        }
        samples.push_back(Sample {
            second,
            rate,
            ttft_ms,
            omitted_hops: answer.hops.len().saturating_sub(HOP_CAP),
            hops: answer.hops.into_iter().take(HOP_CAP).collect(),
        });
    }
    pub fn snapshot(&self, legacy: bool) -> Value {
        self.snapshot_at(
            (self.since_unix_ms + self.started.elapsed().as_millis() as u64) / 1000,
            legacy,
        )
    }
    fn snapshot_at(&self, now: u64, legacy: bool) -> Value {
        let end = now / WINDOW * WINDOW;
        let complete = end.saturating_mul(1000) >= self.since_unix_ms.saturating_add(WINDOW * 1000);
        let day_complete =
            end.saturating_mul(1000) >= self.since_unix_ms.saturating_add(DAY * 1000);
        let mut rows = Vec::new();
        for (id, model) in &self.models {
            let counts = |length: u64| {
                let mut sum = Bucket::default();
                for b in model
                    .buckets
                    .iter()
                    .filter(|b| b.second >= end.saturating_sub(length) && b.second < end)
                {
                    sum.input += b.input;
                    sum.unknown_input += b.unknown_input;
                    sum.output += b.output;
                    sum.verified += b.verified;
                    sum.answers += b.answers;
                    sum.cached += b.cached;
                    sum.untimed_answers_no_worker_timestamps +=
                        b.untimed_answers_no_worker_timestamps;
                    sum.short_answers += b.short_answers;
                    sum.zero_output_answers += b.zero_output_answers;
                    sum.decode_missing_timestamps_answers += b.decode_missing_timestamps_answers;
                    sum.ttft_missing_timestamps_answers += b.ttft_missing_timestamps_answers;
                    sum.invalid_decode_timing_answers += b.invalid_decode_timing_answers;
                    sum.speed_eligible_answers += b.speed_eligible_answers;
                    sum.ttft_eligible_answers += b.ttft_eligible_answers;
                }
                sum
            };
            let recent = counts(WINDOW);
            let daily = counts(DAY);
            let samples: Vec<_> = model
                .samples
                .values()
                .flat_map(|samples| samples.iter())
                .rev()
                .filter(|s| s.second >= end.saturating_sub(WINDOW) && s.second < end)
                .collect();
            let rates: Vec<f64> = samples.iter().filter_map(|s| s.rate).collect();
            let ttft: Vec<f64> = samples.iter().filter_map(|s| s.ttft_ms).collect();
            let mut hops = BTreeMap::<(usize, usize, usize), HopSample>::new();
            let mut omitted_hops: u64 = samples.iter().map(|s| s.omitted_hops as u64).sum();
            for hop in samples.iter().flat_map(|s| &s.hops) {
                let key = (hop.hop, hop.start_layer, hop.end_layer);
                if !hops.contains_key(&key) && hops.len() >= HOP_CAP {
                    omitted_hops += 1;
                    continue;
                }
                let total = hops.entry(key).or_insert(HopSample {
                    hop: hop.hop,
                    start_layer: hop.start_layer,
                    end_layer: hop.end_layer,
                    positions: 0,
                    wall_ms: 0,
                    compute_ms: 0,
                });
                total.positions += hop.positions;
                total.wall_ms += hop.wall_ms;
                total.compute_ms += hop.compute_ms;
            }
            let hops: Vec<_> = hops.into_values().collect();
            if legacy && (!complete || self.rejected_models > 0) {
                continue;
            }
            rows.push(json!({
                "model_id": format!("0x{}", id.iter().map(|b| format!("{b:02x}")).collect::<String>()),
                "model_name": null,
                "answers": complete.then_some(recent.answers),
                "served_tokens": complete.then_some(recent.output),
                "verified_tokens": complete.then_some(recent.verified),
                "served_tokens_per_second": complete.then_some(recent.output as f64 / WINDOW as f64),
                "input_tokens": (complete && recent.unknown_input == 0).then_some(recent.input),
                "input_tokens_per_second": (complete && recent.unknown_input == 0).then_some(recent.input as f64 / WINDOW as f64),
                "output_tokens": complete.then_some(recent.output),
                "output_tokens_per_second": complete.then_some(recent.output as f64 / WINDOW as f64),
                "input_tokens_last_day": (day_complete && daily.unknown_input == 0).then_some(daily.input),
                "output_tokens_last_day": day_complete.then_some(daily.output),
                "day_window_secs": DAY,
                "day_reason": if !day_complete { Some("A full 86400-second window has not been observed since restart.") } else { None },
                "input_reason": (recent.unknown_input > 0 || daily.unknown_input > 0).then_some("Some completed requests lack measured tokenized input counts."),
                "cached_answers": complete.then_some(recent.cached),
                "untimed_answers_no_worker_timestamps": complete.then_some(recent.untimed_answers_no_worker_timestamps),
                "short_answers": complete.then_some(recent.short_answers),
                "zero_output_answers": complete.then_some(recent.zero_output_answers),
                "decode_missing_timestamps_answers": complete.then_some(recent.decode_missing_timestamps_answers),
                "ttft_missing_timestamps_answers": complete.then_some(recent.ttft_missing_timestamps_answers),
                "invalid_decode_timing_answers": complete.then_some(recent.invalid_decode_timing_answers),
                "speed_eligible_answers": complete.then_some(recent.speed_eligible_answers),
                "ttft_eligible_answers": complete.then_some(recent.ttft_eligible_answers),
                "timing_coverage": if complete { json!({
                    "speed": timing_coverage(recent.answers, recent.speed_eligible_answers, rates.len(), &[
                        ("cached_answers", recent.cached), ("short_answers", recent.short_answers),
                        ("missing_token_timestamps", recent.decode_missing_timestamps_answers),
                        ("invalid_decode_interval", recent.invalid_decode_timing_answers)]),
                    "ttft": timing_coverage(recent.answers, recent.ttft_eligible_answers, ttft.len(), &[
                        ("cached_answers", recent.cached), ("zero_output_answers", recent.zero_output_answers),
                        ("missing_first_token_timestamp", recent.ttft_missing_timestamps_answers)]),
                }) } else { Value::Null },
                "answer_tokens_per_second": if complete { rates.clone() } else { vec![] },
                "answer_speed": percentiles(if complete { &rates } else { &[] }),
                "time_to_first_token_ms": percentiles(if complete { &ttft } else { &[] }),
                "ttft_samples_ms": if complete { ttft } else { vec![] },
                "hop_samples": if complete { hops } else { vec![] },
                "omitted_hop_samples": omitted_hops,
                "hop_latency_definition": "Layer-range totals across positions in the sampled answers; wall_ms is the selected successful replica round trip including compute; excludes failed attempts and quorum tail. Sampled, not pure network RTT.",
                "sample_limit": SAMPLE_CAP,
                "ttft_definition": "Server handler entry to first accepted output token; excludes response transport. Remote worker timestamps are unavailable.",
                "sampled_answers": samples.len(),
                "reason": if complete { None } else { Some("A full 60-second window has not been observed since restart.") },
            }));
        }
        json!({
            "schema": if legacy { "arc.community.model-stats.v1" } else { "arc.community.model-stats.v2" },
            "scope": "completed_public_answers_on_this_coordinator",
            "window_secs": WINDOW,
            "routes": ["/inference/run", "/inference/run_sharded", "/inference/run_consensus"],
            "window_start_unix_ms": complete.then_some(end.saturating_sub(WINDOW) * 1000),
            "window_end_unix_ms": end * 1000,
            "since_unix_ms": self.since_unix_ms,
            "as_of_unix_ms": now * 1000,
            "day_window_start_unix_ms": day_complete.then_some(end.saturating_sub(DAY) * 1000),
            "day_window_end_unix_ms": end * 1000,
            "available": complete && self.rejected_models == 0,
            "day_available": day_complete && self.rejected_models == 0,
            "reason": if self.rejected_models > 0 { Some("Model capacity exceeded; this source is incomplete.") }
                else if !complete { Some("A full 60-second window has not been observed since restart.") } else { None },
            "models": rows, "rejected_model_answers": self.rejected_models,
        })
    }
}
fn timing_coverage(
    answers: u64,
    eligible: u64,
    samples: usize,
    exclusions: &[(&str, u64)],
) -> Value {
    let mut reasons: Vec<&str> = exclusions
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(reason, _)| *reason)
        .collect();
    if (samples as u64) < eligible {
        reasons.push("sample_limit");
    }
    json!({ "eligible_answers": eligible, "sampled_answers": samples,
        "total_answers": answers, "partial": (samples as u64) < answers,
        "reasons": reasons })
}
fn percentiles(values: &[f64]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let percentile = |p: f64| -> Option<f64> {
        if sorted.is_empty() {
            return None;
        }
        let rank = (sorted.len() - 1) as f64 * p;
        let low = rank.floor() as usize;
        let high = rank.ceil() as usize;
        Some(sorted[low] + (sorted[high] - sorted[low]) * (rank - low as f64))
    };
    json!({ "samples": sorted.len(), "p5": percentile(0.05), "p50": percentile(0.5), "p95": percentile(0.95),
        "reason": sorted.is_empty().then_some("No measured samples in the complete window; see timing_coverage for exclusions.") })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn answer(output: u64) -> Answer {
        Answer {
            model: [7; 32],
            input: Some(4),
            output,
            verified: true,
            cached: false,
            timing: AnswerTiming {
                first: Some(Duration::from_millis(200)),
                last: Some(Duration::from_millis(1200)),
            },
            hops: vec![HopSample {
                hop: 0,
                start_layer: 0,
                end_layer: 8,
                positions: 5,
                wall_ms: 80,
                compute_ms: 50,
            }],
        }
    }
    #[test]
    fn serving_stats_contract_vectors() {
        let mut stats = ServingStats {
            since_unix_ms: 0,
            ..ServingStats::default()
        };
        stats.record_at(59, answer(999));
        stats.record_at(60, answer(11));
        stats.record_at(119, answer(51));
        stats.record_at(120, answer(999));
        let v = stats.snapshot_at(121, false);
        let m = &v["models"][0];
        assert_eq!(m["served_tokens"], 62);
        assert_eq!(m["input_tokens"], 8);
        assert_eq!(m["answer_tokens_per_second"], json!([50.0, 10.0]));
        assert_eq!(m["answer_speed"]["p50"], 30.0);
        assert_eq!(m["answer_speed"]["p5"], 12.0);
        assert_eq!(m["time_to_first_token_ms"]["p95"], 200.0);
        assert!(m["output_tokens_last_day"].is_null());
        assert_eq!(stats.snapshot_at(59, true)["models"], json!([]));
        assert_eq!(stats.snapshot_at(179, true)["models"], v["models"]);
    }
    #[test]
    fn serving_stats_day_counts_are_not_projections_and_reads_do_not_mutate() {
        let mut stats = ServingStats {
            since_unix_ms: 0,
            ..ServingStats::default()
        };
        stats.record_at(0, answer(10));
        stats.record_at(1, answer(20));
        stats.record_at(DAY, answer(30));
        let v = stats.snapshot_at(DAY + 1, false);
        assert_eq!(v["models"][0]["output_tokens_last_day"], 30);
        assert_eq!(v, stats.snapshot_at(DAY + 1, false));
        assert_eq!(stats.models[&[7; 32]].buckets.len(), 3);
        assert_eq!(
            stats.snapshot_at(DAY * 3, false)["models"][0]["output_tokens_last_day"],
            0
        );
    }
    #[test]
    fn serving_stats_cache_missing_timing_and_single_tokens_do_not_invent_speed() {
        let mut stats = ServingStats {
            since_unix_ms: 0,
            ..ServingStats::default()
        };
        let mut cached = answer(50);
        cached.cached = true;
        stats.record_at(1, cached);
        stats.record_at(2, answer(1));
        let mut unknown = answer(4);
        unknown.timing = AnswerTiming::default();
        unknown.input = None;
        stats.record_at(3, unknown);
        let v = stats.snapshot_at(60, false);
        let m = &v["models"][0];
        assert_eq!(m["served_tokens"], 55);
        assert_eq!(m["answer_speed"]["samples"], 0);
        assert_eq!(m["time_to_first_token_ms"]["samples"], 1);
        assert!(m["input_tokens"].is_null());
    }
    #[test]
    fn serving_stats_retention_is_bounded_without_losing_counts() {
        let mut stats = ServingStats {
            since_unix_ms: 0,
            ..ServingStats::default()
        };
        for _ in 0..SAMPLE_CAP + 10 {
            stats.record_at(1, answer(3));
        }
        let v = stats.snapshot_at(60, false);
        assert_eq!(v["models"][0]["answers"], SAMPLE_CAP + 10);
        assert_eq!(v["models"][0]["answer_speed"]["samples"], SAMPLE_CAP);
        stats.record_at(DAY + WINDOW, answer(3));
        assert_eq!(stats.models[&[7; 32]].buckets.len(), 1);
    }
    #[test]
    fn serving_stats_public_projection_has_no_addresses_or_free_text() {
        let mut stats = ServingStats {
            since_unix_ms: 0,
            ..ServingStats::default()
        };
        stats.record_at(1, answer(11));
        for legacy in [false, true] {
            let v = stats.snapshot_at(60, legacy);
            let row = &v["models"][0];
            assert!(row["model_name"].is_null());
            assert_eq!(
                row["hop_samples"][0]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                vec![
                    "compute_ms",
                    "end_layer",
                    "hop",
                    "positions",
                    "start_layer",
                    "wall_ms"
                ]
            );
            let text = v.to_string();
            for forbidden in [
                "socket",
                "hostname",
                "worker_id",
                "request_id",
                "127.0.0.1",
                "http://",
                "served_by",
            ] {
                assert!(!text.contains(forbidden), "{forbidden}");
            }
        }
    }
    #[test]
    fn serving_stats_shared_contract_vectors() {
        let fixtures: Vec<Value> =
            serde_json::from_str(include_str!("../../tests/fixtures/serving-stats-v2.json"))
                .unwrap();
        for (fixture, outputs) in fixtures.iter().zip([vec![11, 12, 13], vec![51]]) {
            let mut stats = ServingStats {
                since_unix_ms: 0,
                ..ServingStats::default()
            };
            for output in outputs {
                stats.record_at(1, answer(output));
            }
            let snapshot = stats.snapshot_at(60, false);
            for (key, expected) in fixture.as_object().unwrap() {
                if key == "models" {
                    continue;
                }
                assert_eq!(&snapshot[key], expected, "{key}");
            }
            for (key, expected) in fixture["models"][0].as_object().unwrap() {
                assert_eq!(&snapshot["models"][0][key], expected, "{key}");
            }
        }
    }

    #[test]
    fn serving_stats_clock_origin_partial_second_and_capacity() {
        let mut stats = ServingStats {
            since_unix_ms: 500,
            ..ServingStats::default()
        };
        stats.record_at(1, answer(1));
        assert_eq!(stats.snapshot_at(60, false)["available"], false);
        assert_eq!(stats.snapshot_at(119, false)["available"], false);
        assert_eq!(stats.snapshot_at(120, false)["available"], true);
        for id in 0..MODEL_CAP + 1 {
            let mut a = answer(1);
            a.model = [id as u8; 32];
            stats.record_at(2, a);
        }
        assert_eq!(stats.models.len(), MODEL_CAP);
        assert_eq!(stats.snapshot_at(61, false)["available"], false);
        assert_eq!(stats.snapshot_at(61, true)["models"], json!([]));
    }

    #[test]
    fn serving_stats_staggered_coordinators_and_minute_boundaries() {
        let mut a = ServingStats {
            since_unix_ms: 0,
            ..ServingStats::default()
        };
        let mut b = ServingStats {
            since_unix_ms: 500,
            ..ServingStats::default()
        };
        for stats in [&mut a, &mut b] {
            stats.record_at(60, answer(11));
            stats.record_at(119, answer(12));
            stats.record_at(120, answer(100));
        }
        // Independent origins, reads two seconds apart; both report [60,120).
        for (stats, now) in [(&a, 130), (&b, 132)] {
            let v = stats.snapshot_at(now, false);
            assert_eq!(v["window_start_unix_ms"], 60000);
            assert_eq!(v["window_end_unix_ms"], 120000);
            assert_eq!(v["models"][0]["served_tokens"], 23);
        }
        assert_eq!(a.snapshot_at(179, false)["window_end_unix_ms"], 120000);
        assert_eq!(b.snapshot_at(180, false)["window_end_unix_ms"], 180000);
        assert_eq!(b.snapshot_at(180, false)["models"][0]["served_tokens"], 100);
    }

    #[test]
    fn serving_stats_completed_window_survives_current_minute_writes() {
        let mut stats = ServingStats {
            since_unix_ms: 0,
            ..ServingStats::default()
        };
        stats.record_at(0, answer(11));
        stats.record_at(DAY - 60, answer(12));
        stats.record_at(DAY - 1, answer(13));
        for _ in 0..SAMPLE_CAP + 10 {
            stats.record_at(DAY + 59, answer(99));
        }
        let v = stats.snapshot_at(DAY + 59, false);
        assert_eq!(v["models"][0]["output_tokens_last_day"], 36);
        assert_eq!(v["models"][0]["served_tokens"], 25);
        assert_eq!(
            v["models"][0]["answer_tokens_per_second"],
            json!([12.0, 11.0])
        );
        assert_eq!(v["day_window_start_unix_ms"], 0);
        assert_eq!(v["day_window_end_unix_ms"], DAY * 1000);
        assert_eq!(
            stats.models[&[7; 32]]
                .samples
                .values()
                .map(VecDeque::len)
                .sum::<usize>(),
            SAMPLE_CAP + 2
        );
    }

    #[test]
    fn serving_stats_mixed_timing_coverage_is_counted_before_sampling() {
        let mut stats = ServingStats {
            since_unix_ms: 0,
            ..ServingStats::default()
        };
        stats.record_at(1, answer(11));
        stats.record_at(2, answer(1)); // TTFT yes, decode no.
        stats.record_at(3, answer(0));
        let mut cached = answer(10);
        cached.cached = true;
        stats.record_at(4, cached);
        for output in [1, 10] {
            let mut untimed = answer(output);
            untimed.timing = AnswerTiming::default();
            stats.record_at(5, untimed);
        }
        let mut invalid = answer(10);
        invalid.timing.last = invalid.timing.first;
        stats.record_at(6, invalid);
        let v = stats.snapshot_at(60, false);
        let m = &v["models"][0];
        assert_eq!(m["answers"], 7);
        assert_eq!(m["untimed_answers_no_worker_timestamps"], 2);
        assert_eq!(m["short_answers"], 3);
        assert_eq!(m["speed_eligible_answers"], 1);
        assert_eq!(m["ttft_eligible_answers"], 3);
        assert_eq!(m["timing_coverage"]["speed"]["partial"], true);
        assert_eq!(
            m["timing_coverage"]["speed"]["reasons"],
            json!([
                "cached_answers",
                "short_answers",
                "missing_token_timestamps",
                "invalid_decode_interval"
            ])
        );
        assert_eq!(
            m["timing_coverage"]["ttft"]["reasons"],
            json!([
                "cached_answers",
                "zero_output_answers",
                "missing_first_token_timestamp"
            ])
        );
        for _ in 0..SAMPLE_CAP {
            stats.record_at(7, answer(11));
        }
        let v = stats.snapshot_at(60, false);
        let m = &v["models"][0];
        assert_eq!(m["answers"], SAMPLE_CAP + 7);
        assert_eq!(m["untimed_answers_no_worker_timestamps"], 2);
        assert_eq!(m["speed_eligible_answers"], SAMPLE_CAP + 1);
        assert!(
            m["timing_coverage"]["speed"]["reasons"]
                .as_array()
                .unwrap()
                .contains(&json!("sample_limit"))
        );
    }

    #[tokio::test]
    async fn serving_stats_v1_unavailable_is_http_error_for_both_aliases() {
        use super::super::{serving_stats_routes, tests::fake_node_with_workers};
        use axum::{
            body::{Body, to_bytes},
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;
        let fixtures: Vec<Value> = serde_json::from_str(include_str!(
            "../../tests/fixtures/serving-stats-v1-unavailable.json"
        ))
        .unwrap();
        for fixture in fixtures {
            let capacity_rejected = fixture["capacity_rejected"].as_bool().unwrap();
            let node = fake_node_with_workers(Vec::new());
            if capacity_rejected {
                *node.serving_stats.lock() = ServingStats {
                    started: Instant::now() - Duration::from_secs(120),
                    since_unix_ms: 0,
                    ..ServingStats::default()
                };
                for id in 0..=MODEL_CAP {
                    let mut a = answer(1);
                    a.model = [id as u8; 32];
                    node.serving_stats.lock().record_at(61, a);
                }
            }
            let app = serving_stats_routes().with_state(node);
            for alias in ["/community/model_stats", "/inference/model_stats"] {
                for suffix in ["", "/v2"] {
                    let response = app
                        .clone()
                        .oneshot(
                            Request::builder()
                                .uri(format!("{alias}{suffix}"))
                                .body(Body::empty())
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(
                        response.status(),
                        if suffix.is_empty() {
                            StatusCode::SERVICE_UNAVAILABLE
                        } else {
                            StatusCode::OK
                        }
                    );
                    let body: Value = serde_json::from_slice(
                        &to_bytes(response.into_body(), 1_000_000).await.unwrap(),
                    )
                    .unwrap();
                    if suffix.is_empty() {
                        assert_eq!(fixture["status"], StatusCode::SERVICE_UNAVAILABLE.as_u16());
                        for (key, expected) in fixture["body"].as_object().unwrap() {
                            assert_eq!(&body[key], expected);
                        }
                    }
                    assert_eq!(body["available"], false);
                    assert!(
                        body["reason"]
                            .as_str()
                            .unwrap()
                            .contains(if capacity_rejected {
                                "capacity"
                            } else {
                                "restart"
                            })
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn serving_stats_routes_are_get_only_and_private_fields_never_escape() {
        use super::super::{serving_stats_routes, tests::fake_node_with_workers};
        use axum::{
            body::{Body, to_bytes},
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;
        let node = fake_node_with_workers(Vec::new());
        *node.serving_stats.lock() = ServingStats {
            started: Instant::now() - Duration::from_secs(60),
            since_unix_ms: 0,
            ..ServingStats::default()
        };
        node.serving_stats.lock().record_at(1, answer(11));
        // Sensitive unrelated state cannot leak through the public projection.
        node.latency_stats.insert(
            "private-worker.internal:9944".into(),
            super::super::LatencyEWMA {
                ms: 1.0,
                count: 1,
                last_updated: Instant::now(),
                probe_only: false,
            },
        );
        let app = serving_stats_routes().with_state(node.clone());
        for path in [
            "/community/model_stats",
            "/inference/model_stats",
            "/community/model_stats/v2",
            "/inference/model_stats/v2",
        ] {
            let before = node.serving_stats.lock().snapshot_at(60, false);
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
            let text = std::str::from_utf8(&bytes).unwrap();
            assert!(!text.contains("private-worker"));
            assert_eq!(before, node.serving_stats.lock().snapshot_at(60, false));
            assert!(
                node.latency_stats
                    .contains_key("private-worker.internal:9944")
            );
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        }
    }
}
