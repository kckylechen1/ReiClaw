//! Shared repetition guarantees migrated from the legacy detector and
//! result collector, plus the unified guard's bounded advisory episodes.

use super::*;
use serde_json::json;

fn record(
    guard: &mut RepetitionGuard,
    pacing: &PacingConfig,
    name: &str,
    arguments: serde_json::Value,
    output: &str,
    success: bool,
) -> RepetitionDisposition {
    guard.observe_round(
        &[ParsedToolCall {
            name: name.into(),
            arguments,
            tool_call_id: Some("call".into()),
        }],
        &[CompletedToolExecution {
            executed_tool_name: Some(name.into()),
            outcome: ToolExecutionOutcome {
                output: output.into(),
                success,
                error_reason: (!success).then(|| output.into()),
                duration: Duration::ZERO,
                receipt: None,
            },
        }],
        &HashSet::new(),
        pacing,
    )
}

#[test]
fn successful_first_call_is_not_a_repeat_even_with_a_low_threshold() {
    for max_repeats in [0, 1] {
        let pacing = PacingConfig {
            loop_detection_max_repeats: max_repeats,
            ..PacingConfig::default()
        };
        let mut guard = RepetitionGuard::default();
        let first = record(&mut guard, &pacing, "tool", json!({}), "ok", true);
        assert!(first.message.is_none());
        assert!(!first.close);
        assert_eq!(
            record(&mut guard, &pacing, "tool", json!({}), "ok", true).message,
            Some(SUCCESS_ADVISORY),
        );
    }
}

#[test]
fn exact_success_repeats_advise_once_at_threshold_and_never_abort() {
    let mut guard = RepetitionGuard::default();
    let pacing = PacingConfig::default();
    for i in 0..20 {
        let result = record(
            &mut guard,
            &pacing,
            "read",
            json!({"path":"a"}),
            "data",
            true,
        );
        assert_eq!(result.message, (i == 2).then_some(SUCCESS_ADVISORY));
        assert!(!result.close);
    }
    assert!(!guard.is_exhausted());
}

#[test]
fn exact_repeat_resets_on_a_different_completed_call() {
    let mut guard = RepetitionGuard::default();
    let pacing = PacingConfig::default();
    for _ in 0..2 {
        assert!(
            record(&mut guard, &pacing, "a", json!({}), "r1", true)
                .message
                .is_none()
        );
    }
    assert!(
        record(&mut guard, &pacing, "b", json!({}), "r2", true)
            .message
            .is_none()
    );
    assert!(
        record(&mut guard, &pacing, "a", json!({}), "r1", true)
            .message
            .is_none()
    );
    assert!(
        record(
            &mut guard,
            &pacing,
            "a",
            json!({"x":999}),
            "different",
            true
        )
        .message
        .is_none()
    );
}

#[test]
fn narrow_windows_do_not_disable_exact_failure_recovery() {
    let mut guard = RepetitionGuard::default();
    let pacing = PacingConfig {
        loop_detection_window_size: 2,
        ..PacingConfig::default()
    };
    for i in 0..4 {
        let result = record(&mut guard, &pacing, "tool", json!({}), "failed", false);
        assert_eq!(result.close, i == 3);
        assert_eq!(
            result.message,
            match i {
                2 => Some(FAILURE_RECOVERY),
                3 => Some(FAILURE_EXHAUSTED),
                _ => None,
            }
        );
    }
}

#[test]
fn ping_pong_advises_at_four_cycles_with_varying_arguments_without_escalating() {
    let mut guard = RepetitionGuard::default();
    let pacing = PacingConfig::default();
    for i in 0..30 {
        let name = if i % 2 == 0 { "read" } else { "write" };
        let result = record(
            &mut guard,
            &pacing,
            name,
            json!({"attempt":i}),
            &format!("r{i}"),
            true,
        );
        assert_eq!(result.message, (i == 7).then_some(PING_PONG_ADVISORY));
        assert!(!result.close);
    }
    assert_eq!(guard.advised_patterns.len(), 1);
    assert!(!guard.is_exhausted());
}

#[test]
fn one_tool_does_not_form_ping_pong_and_changed_results_are_progress() {
    let mut guard = RepetitionGuard::default();
    let pacing = PacingConfig::default();
    for i in 0..12 {
        let result = record(
            &mut guard,
            &pacing,
            "read",
            json!({"path":i}),
            &format!("r{i}"),
            true,
        );
        assert!(result.message.is_none());
        assert!(!result.close);
    }
}

#[test]
fn no_progress_counts_interleaved_calls_with_distinct_arguments_and_advises_once() {
    for interleave in [false, true] {
        let mut guard = RepetitionGuard::default();
        let pacing = PacingConfig::default();
        for i in 0..8 {
            let result = record(
                &mut guard,
                &pacing,
                "search",
                json!({"q":i}),
                "no results",
                true,
            );
            assert_eq!(result.message, (i == 4).then_some(NO_PROGRESS_ADVISORY));
            assert!(!result.close);
            if interleave {
                // Vary unrelated tool identities so this cannot form ping-pong.
                assert!(
                    record(
                        &mut guard,
                        &pacing,
                        &format!("other-{i}"),
                        json!({}),
                        "other",
                        true
                    )
                    .message
                    .is_none()
                );
            }
        }
        assert!(!guard.is_exhausted());
    }
}

#[test]
fn same_arguments_use_exact_advice_and_never_no_progress_advice() {
    let mut guard = RepetitionGuard::default();
    let pacing = PacingConfig {
        loop_detection_max_repeats: 6,
        ..PacingConfig::default()
    };
    for i in 0..8 {
        let result = record(
            &mut guard,
            &pacing,
            "search",
            json!({"q":"same"}),
            "none",
            true,
        );
        assert_eq!(result.message, (i == 5).then_some(SUCCESS_ADVISORY));
        assert!(!result.close);
    }
}

#[test]
fn failed_calls_with_different_arguments_do_not_feed_success_patterns_or_timed_advice() {
    for elapsed in [None, Some(0)] {
        let mut guard = RepetitionGuard::default();
        let pacing = PacingConfig {
            loop_detection_min_elapsed_secs: elapsed,
            ..PacingConfig::default()
        };
        for i in 0..8 {
            let result = record(
                &mut guard,
                &pacing,
                "read",
                json!({"path":i}),
                "Rate limit exceeded",
                false,
            );
            assert!(result.message.is_none());
            assert!(!result.close);
        }
        assert!(guard.successful.is_empty());
        assert!(guard.timed_round.is_none());
    }
}

#[test]
fn disabled_guard_does_not_accumulate_success_or_failure_evidence() {
    for success in [false, true] {
        let mut guard = RepetitionGuard::default();
        let pacing = PacingConfig {
            loop_detection_enabled: false,
            loop_detection_min_elapsed_secs: Some(0),
            ..PacingConfig::default()
        };
        for _ in 0..20 {
            let result = record(&mut guard, &pacing, "tool", json!({}), "same", success);
            assert!(result.message.is_none());
            assert!(!result.close);
        }
        assert!(guard.last.is_none());
        assert!(guard.successful.is_empty());
        assert!(guard.timed_round.is_none());
    }
}

#[test]
fn successful_window_is_bounded_and_eviction_removes_stale_patterns() {
    let mut guard = RepetitionGuard::default();
    let pacing = PacingConfig {
        loop_detection_window_size: 5,
        ..PacingConfig::default()
    };
    for i in 0..6 {
        record(
            &mut guard,
            &pacing,
            &format!("tool-{i}"),
            json!({}),
            "same",
            true,
        );
    }
    assert_eq!(guard.successful.len(), 5);
    assert_eq!(
        guard.successful.front().unwrap().tool,
        hash_tool_name("tool-1")
    );
    for i in 0..4 {
        assert!(
            record(&mut guard, &pacing, "search", json!({"q":i}), "none", true)
                .message
                .is_none()
        );
    }
    for i in 0..5 {
        record(
            &mut guard,
            &pacing,
            &format!("fresh-{i}"),
            json!({}),
            "other",
            true,
        );
    }
    assert!(
        record(
            &mut guard,
            &pacing,
            "search",
            json!({"q":"new"}),
            "none",
            true
        )
        .message
        .is_none()
    );
    assert_eq!(guard.successful.len(), 5);
}

#[test]
fn pattern_advisory_rearms_only_after_its_bounded_episode_is_evicted() {
    let mut guard = RepetitionGuard::default();
    let pacing = PacingConfig::default();
    for cycle in 0..2 {
        for i in 0..12 {
            let name = if i % 2 == 0 { "read" } else { "write" };
            let result = record(
                &mut guard,
                &pacing,
                name,
                json!({"i":i}),
                &format!("r{i}"),
                true,
            );
            assert_eq!(
                result.message,
                (i == 7).then_some(PING_PONG_ADVISORY),
                "episode {cycle}"
            );
        }
        for i in 0..pacing.loop_detection_window_size {
            record(
                &mut guard,
                &pacing,
                &format!("other-{i}"),
                json!({}),
                "other",
                true,
            );
        }
        assert!(guard.advised_patterns.is_empty());
    }
}

#[test]
fn timed_advisory_keeps_elapsed_grace_and_emits_once_per_unchanged_episode() {
    let mut guard = RepetitionGuard::default();
    let pacing = PacingConfig {
        loop_detection_min_elapsed_secs: Some(30),
        loop_detection_max_repeats: 100,
        ..PacingConfig::default()
    };
    for _ in 0..5 {
        assert!(
            record(&mut guard, &pacing, "status", json!({}), "pending", true)
                .message
                .is_none()
        );
    }
    assert!(
        guard.timed_round.is_none(),
        "no observations counted before grace elapsed"
    );
    guard.started_at = Instant::now() - Duration::from_secs(30);
    for output in ["pending", "running"] {
        for i in 0..12 {
            let result = record(&mut guard, &pacing, "status", json!({}), output, true);
            assert_eq!(result.message, (i == 3).then_some(TIMED_ADVISORY));
            assert!(!result.close);
        }
    }
    assert!(!guard.is_exhausted());
}

#[test]
fn unconfigured_elapsed_gate_does_not_add_timed_advice() {
    let mut guard = RepetitionGuard::default();
    let pacing = PacingConfig {
        loop_detection_max_repeats: 100,
        ..PacingConfig::default()
    };
    for _ in 0..12 {
        assert!(
            record(&mut guard, &pacing, "status", json!({}), "pending", true)
                .message
                .is_none()
        );
    }
    assert!(guard.timed_round.is_none());
}

#[test]
fn empty_successful_outputs_do_not_seed_the_timed_episode() {
    let mut guard = RepetitionGuard::default();
    let pacing = PacingConfig {
        loop_detection_min_elapsed_secs: Some(0),
        loop_detection_max_repeats: 100,
        ..PacingConfig::default()
    };
    for _ in 0..8 {
        assert!(
            record(&mut guard, &pacing, "status", json!({}), "", true)
                .message
                .is_none()
        );
    }
    assert!(guard.timed_round.is_none());
}

fn argument_hash(value: &serde_json::Value) -> u64 {
    call_signature("remote", value)
}

#[test]
fn argument_hash_is_key_order_independent() {
    let a = json!({"alpha": 1, "beta": 2});
    let b = json!({"beta": 2, "alpha": 1});
    assert_eq!(
        argument_hash(&a),
        argument_hash(&b),
        "argument_hash must produce identical hashes regardless of JSON key order"
    );
}

#[test]
fn argument_hash_nested_key_order_independent() {
    let a = json!({"outer": {"x": 1, "y": 2}, "z": [1, 2]});
    let b = json!({"z": [1, 2], "outer": {"y": 2, "x": 1}});
    assert_eq!(
        argument_hash(&a),
        argument_hash(&b),
        "nested objects must also be key-order independent"
    );
}

#[test]
fn argument_hash_distinguishes_string_from_number_and_bool() {
    // JSON types remain distinct in the canonical call signature.
    let num = json!(1);
    let str_num = json!("1");
    let truthy = json!(true);
    assert_ne!(argument_hash(&num), argument_hash(&str_num));
    assert_ne!(argument_hash(&num), argument_hash(&truthy));
    assert_ne!(argument_hash(&str_num), argument_hash(&truthy));
    assert_ne!(argument_hash(&json!(null)), argument_hash(&json!("null")));
}

#[test]
fn argument_hash_distinguishes_array_ordering() {
    // Arrays are order-sensitive; [1, 2] and [2, 1] must hash differently.
    assert_ne!(
        argument_hash(&json!([1, 2])),
        argument_hash(&json!([2, 1])),
        "array ordering is part of the value and must affect the hash"
    );
}

#[test]
fn argument_hash_handles_large_args_without_panicking() {
    // Large arguments remain deterministic without changing their values.
    let big_payload = "x".repeat(1024 * 1024);
    let args = json!({ "body": big_payload, "kind": "echo" });
    let h1 = argument_hash(&args);
    let h2 = argument_hash(&args);
    assert_eq!(h1, h2, "hash must be deterministic for the same input");

    // And a permuted key order over the same payload must match.
    let args_permuted = json!({ "kind": "echo", "body": big_payload });
    assert_eq!(
        h1,
        argument_hash(&args_permuted),
        "key-order independence must hold for large payloads too"
    );
}

// ── Legacy equivalence oracle ─────────────────────────────────

/// Independent test-only oracle retained from the removed detector.
fn legacy_canonicalise(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            sorted.sort_by_key(|(k, _)| *k);
            let new_map: serde_json::Map<String, serde_json::Value> = sorted
                .into_iter()
                .map(|(k, v)| (k.clone(), legacy_canonicalise(v)))
                .collect();
            serde_json::Value::Object(new_map)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(legacy_canonicalise).collect())
        }
        other => other.clone(),
    }
}

/// Legacy hash path: canonicalise → `serde_json::to_string` → `DefaultHasher`.
fn legacy_hash(value: &serde_json::Value) -> u64 {
    use std::hash::Hasher;
    let canonical = serde_json::to_string(&legacy_canonicalise(value)).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    hasher.finish()
}

/// Equivalence battery: the canonical `argument_hash` must classify the same
/// value pairs as equal or different as the legacy `canonicalise` +
/// `serde_json::to_string` + hash path across nested objects, arrays of
/// objects, large and escaped strings, numbers, booleans, nulls, and mixed
/// values.
#[test]
fn call_signature_preserves_legacy_json_equivalence() {
    let cases: &[(&str, serde_json::Value, serde_json::Value, bool)] = &[
        // Same logical content, different key order → equal
        (
            "key-order",
            json!({"alpha": 1, "beta": 2}),
            json!({"beta": 2, "alpha": 1}),
            true,
        ),
        // Nested objects, key-order invariant
        (
            "nested-objects",
            json!({"outer": {"inner": {"a": 1, "b": 2}}}),
            json!({"outer": {"inner": {"b": 2, "a": 1}}}),
            true,
        ),
        // Arrays of objects
        (
            "array-of-objects",
            json!({"items": [{"x": 1, "y": 2}, {"x": 3, "y": 4}]}),
            json!({"items": [{"y": 2, "x": 1}, {"y": 4, "x": 3}]}),
            true,
        ),
        // Mixed values: null, bool, number, string
        (
            "mixed-leaves",
            json!({"n": null, "b": true, "num": 42, "s": "hello"}),
            json!({"s": "hello", "b": true, "n": null, "num": 42}),
            true,
        ),
        // Different string values → not equal
        (
            "different-string",
            json!({"x": "a"}),
            json!({"x": "b"}),
            false,
        ),
        // Different number → not equal
        ("different-number", json!({"x": 1}), json!({"x": 2}), false),
        // Different boolean → not equal
        (
            "different-bool",
            json!({"x": true}),
            json!({"x": false}),
            false,
        ),
        // null vs string → not equal
        (
            "null-vs-string",
            json!({"x": null}),
            json!({"x": "null"}),
            false,
        ),
        // Nested difference → not equal
        (
            "nested-diff",
            json!({"a": {"b": 1}}),
            json!({"a": {"b": 2}}),
            false,
        ),
        // Array ordering matters → not equal
        ("array-ordering", json!([1, 2, 3]), json!([3, 2, 1]), false),
        // Large escaped string
        (
            "large-string",
            json!({"data": "x".repeat(10000) + "\"escaped\"\n\t\\"}),
            json!({"data": "x".repeat(10000) + "\"escaped\"\n\t\\"}),
            true,
        ),
        // Large string differs at end
        (
            "large-string-diff",
            json!({"data": "x".repeat(10000) + "a"}),
            json!({"data": "x".repeat(10000) + "b"}),
            false,
        ),
        // Numeric equivalence battery. The legacy path serialised
        // numbers via serde_json, so `0`, `0.0`, and `-0.0` produced
        // distinct tokens `0`, `0.0`, `-0.0`. The previous
        // `Number::hash` impl routed integer 0 and float 0.0 through
        // `0.0f64.to_bits()` (colliding) and normalized `+0.0` /
        // `-0.0`. After the number-text fix above these cases
        // reclassify in line with legacy equality.
        (
            "int-zero-vs-float-zero",
            json!({"x": 0}),
            json!({"x": 0.0}),
            false,
        ),
        (
            "positive-zero-vs-negative-zero",
            json!({"x": 0.0}),
            json!({"x": -0.0}),
            false,
        ),
        (
            "int-zero-vs-negative-zero",
            json!({"x": 0}),
            json!({"x": -0.0}),
            false,
        ),
        (
            "same-float-text",
            json!({"x": 1.5}),
            json!({"x": 1.5}),
            true,
        ),
        (
            "different-float-text",
            json!({"x": 1.5}),
            json!({"x": 2.5}),
            false,
        ),
        (
            "int-vs-float-one",
            json!({"x": 1}),
            json!({"x": 1.0}),
            false,
        ),
        (
            "int-vs-negative-float",
            json!({"x": -1}),
            json!({"x": -1.0}),
            false,
        ),
        (
            "large-int-vs-large-float",
            json!({"x": 123456789012345_i64}),
            json!({"x": 123456789012345.0}),
            false,
        ),
    ];

    for (label, a, b, expect_equal) in cases {
        let actual_a = argument_hash(a);
        let actual_b = argument_hash(b);
        let legacy_a = legacy_hash(a);
        let legacy_b = legacy_hash(b);

        let actual_eq = actual_a == actual_b;
        let legacy_eq = legacy_a == legacy_b;

        assert_eq!(
            actual_eq, legacy_eq,
            "{label}: canonical ({actual_a} vs {actual_b}) and legacy ({legacy_a} vs {legacy_b}) must agree on equality"
        );
        assert_eq!(
            actual_eq, *expect_equal,
            "{label}: expected equal={expect_equal}, got actual_eq={actual_eq}"
        );
    }
}
