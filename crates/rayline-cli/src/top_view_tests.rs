use super::*;
use serde_json::json;

/// 2026-11-18 14:35:20 UTC.
const NOW: u64 = 1_795_012_520_000;

/// The running process's local deployments: `qwen3.5-35b` on the `spark`
/// endpoint.
static ENDPOINTS: std::sync::LazyLock<std::collections::BTreeMap<String, String>> =
    std::sync::LazyLock::new(|| {
        [("qwen3.5-35b-chat".to_owned(), "spark".to_owned())]
            .into_iter()
            .collect()
    });

fn context() -> Context<'static> {
    Context {
        endpoints: &ENDPOINTS,
        now_unix_ms: NOW,
        utc_offset_s: |_| 0,
        local_models: false,
        on_mac: true,
        tick: 0,
    }
}

/// A local process that has run for 12 minutes, deciding on this machine,
/// with a request in flight and four finished ones.
fn snapshot() -> Value {
    json!({
        "runtime": "rayline-local",
        "started_at_unix_ms": NOW - 12 * 60_000 - 5_000,
        "router": {"kind": "local", "model": "arc-swift-1.0", "preset": "arc-policy.prefer-local.v1"},
        "totals": {
            "active_requests": 1, "completed_requests": 3, "errored_requests": 1,
            "local_requests": 1, "cloud_requests": 3,
            "input_tokens": 35_200, "output_tokens": 1_400, "cache_read_tokens": 25_344
        },
        "active": [{
            "request_id": "req_inflight", "state": "streaming", "harness": "codex",
            "router": "local", "decision_ms": 95, "decision_event": "tool_result",
            "started_at_unix_ms": NOW - 3_200, "duration_ms": 3_200,
            "target": "local", "endpoint_id": "qwen3.5-35b-chat", "executed_model": "qwen3.5-35b"
        }],
        "recent": [
            {
                "request_id": "req_1a1ea7bf3", "state": "completed", "harness": "claude",
                "router": "local", "decision_ms": 122, "decision_event": "task_start",
                "decision_model": "claude-haiku-5.5", "thinking_level": "none",
                "decision_reason": "utility_fallback",
                "arc_encoded_tokens": 20_940, "arc_restored_tokens": 8_286,
                "started_at_unix_ms": NOW - 20_000, "completed_at_unix_ms": NOW - 17_000,
                "duration_ms": 3_000, "target": "cloud", "executed_model": "claude-haiku-5.5",
                "input_tokens": 12_100, "prompt_cache_tokens": 9_800, "cache_write_tokens": 1_900,
                "cache_hit_ratio": 0.81, "output_tokens": 888, "ttft_ms": 900, "output_tps": 284.3
            },
            {
                "request_id": "req_local", "state": "completed", "harness": "codex",
                "router": "local", "decision_ms": 95, "decision_event": "tool_result",
                "decision_model": "qwen3.5-35b", "thinking_level": "low",
                "decision_reason": "prefer_local_threshold", "p_pass": 0.82, "min_p_pass": 0.75,
                "started_at_unix_ms": NOW - 80_000, "completed_at_unix_ms": NOW - 70_000,
                "duration_ms": 10_000, "target": "local", "endpoint_id": "qwen3.5-35b-chat",
                "executed_model": "qwen3.5-35b", "input_tokens": 11_600,
                "prompt_cache_tokens": 10_904, "cache_hit_ratio": 0.94,
                "output_tokens": 118, "ttft_ms": 1_600, "output_tps": 31.0
            },
            {
                "request_id": "req_classifier", "state": "completed", "harness": "claude",
                "router": null, "started_at_unix_ms": NOW - 140_000,
                "completed_at_unix_ms": NOW - 139_000, "duration_ms": 1_000,
                "target": "cloud", "executed_model": "claude-haiku-5.5",
                "input_tokens": 700, "output_tokens": 30, "ttft_ms": 400
            },
            {
                "request_id": "req_failed", "state": "error", "harness": null,
                "router": "local", "decision_ms": 496, "decision_event": "task_start",
                "started_at_unix_ms": NOW - 200_000, "completed_at_unix_ms": NOW - 199_000,
                "duration_ms": 1_000, "target": "cloud", "executed_model": "claude-haiku-5.5",
                "status_code": 529
            }
        ]
    })
}

fn render(snapshot: &Value, view: &ViewState, width: usize, height: usize) -> String {
    frame(Some(snapshot), view, &context(), &[], width, height)
        .iter()
        .map(|line| {
            let mark = if line.style == Style::Selected {
                '>'
            } else {
                '|'
            };
            format!("{mark}{}\n", line.text.trim_end())
        })
        .collect()
}

/// Compares `actual` to the file `name` in `top_view_snapshots`. With
/// `RAYLINE_UPDATE_SNAPSHOTS=1` it writes the file instead.
fn assert_snapshot(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/top_view_snapshots")
        .join(name);
    if std::env::var_os("RAYLINE_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(actual, expected, "{name} differs; actual:\n{actual}");
}

#[test]
fn the_live_view_at_80_100_and_140_columns() {
    let view = ViewState {
        selected: Some("req_1a1ea7bf3".to_owned()),
        ..ViewState::default()
    };
    for width in [80, 100, 140] {
        let rendered = render(&snapshot(), &view, width, 18);
        for line in rendered.lines() {
            assert!(cell_count(line) <= width + 1, "{width}: {line}");
        }
        assert_snapshot(&format!("live-{width}.txt"), &rendered);
    }
}

#[test]
fn narrow_terminals_drop_columns_in_order() {
    let names = |width| {
        let (shown, _) = layout(width);
        shown
            .iter()
            .map(|index| COLUMNS[*index].name)
            .collect::<Vec<_>>()
    };
    assert_eq!(names(140).len(), 11);
    let medium = names(100);
    assert!(!medium.contains(&"TOK/S") && !medium.contains(&"TTFT"));
    assert!(medium.contains(&"CACHED") && medium.contains(&"IN"));
    let narrow = names(80);
    for dropped in ["TOK/S", "TTFT", "CACHED", "IN"] {
        assert!(!narrow.contains(&dropped), "{narrow:?}");
    }
    assert!(narrow.contains(&"TOOL") && narrow.contains(&"MODEL"));
    let tiny = names(40);
    assert!(!tiny.contains(&"TOOL"));
    for width in [40, 60, 80, 100, 140, 200] {
        let (_, model) = layout(width);
        assert!((MODEL_MIN..=MODEL_MAX).contains(&model), "{width}");
    }
}

#[test]
fn the_details_view() {
    let view = ViewState {
        selected: Some("req_1a1ea7bf3".to_owned()),
        details: true,
        ..ViewState::default()
    };
    assert_snapshot("details-local.txt", &render(&snapshot(), &view, 100, 12));
    // A cloud decision has no reuse count; a local server is named.
    let mut cloud = snapshot();
    cloud["router"]["kind"] = json!("cloud");
    cloud["recent"][1]["router"] = json!("cloud");
    cloud["recent"][1]["decision_ms"] = json!(210);
    let view = ViewState {
        selected: Some("req_local".to_owned()),
        details: true,
        ..ViewState::default()
    };
    assert_snapshot("details-cloud.txt", &render(&cloud, &view, 100, 12));
}

#[test]
fn why_lines_name_the_reason_in_words() {
    let rows = snapshot();
    let with_models = Context {
        local_models: true,
        ..context()
    };
    assert_eq!(
        why(&rows["recent"][0], &context()).as_deref(),
        Some("best value for this task · no local model configured")
    );
    assert_eq!(
        why(&rows["recent"][0], &with_models).as_deref(),
        Some("best value for this task")
    );
    assert_eq!(
        why(&rows["recent"][1], &context()).as_deref(),
        Some("local model good enough (pass 0.82 ≥ 0.75) · thinking low")
    );
    assert_eq!(why(&rows["recent"][2], &context()), None);
    let scheduled = json!({"decision_reason": "schedule", "thinking_level": "high"});
    assert_eq!(
        why(&scheduled, &context()).as_deref(),
        Some("scheduled · thinking high")
    );
}

#[test]
fn the_filter_cycles_through_all_requests_then_each_harness() {
    let snapshot = snapshot();
    assert_eq!(harnesses(&snapshot), ["claude", "codex"]);
    assert_eq!(next_filter(&snapshot, None).as_deref(), Some("claude"));
    assert_eq!(
        next_filter(&snapshot, Some("claude")).as_deref(),
        Some("codex")
    );
    assert_eq!(next_filter(&snapshot, Some("codex")), None);
    let ids = |filter| {
        list_rows(&snapshot, filter)
            .iter()
            .map(|row| row["request_id"].as_str().unwrap())
            .collect::<Vec<_>>()
    };
    // Newest first, in flight before finished.
    assert_eq!(
        ids(None),
        [
            "req_inflight",
            "req_1a1ea7bf3",
            "req_local",
            "req_classifier",
            "req_failed"
        ]
    );
    assert_eq!(ids(Some("codex")), ["req_inflight", "req_local"]);
}

#[test]
fn the_selected_row_stays_in_view() {
    let view = ViewState {
        selected: Some("req_failed".to_owned()),
        ..ViewState::default()
    };
    // Room for the column names and two rows.
    let lines = list(&snapshot(), &view, &context(), 140, 3);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[2].style, Style::Selected);
    assert!(lines[2].text.contains("14:32"), "{}", lines[2].text);
    // Unselected, the failed request is drawn as an error; a local one as local.
    let lines = list(&snapshot(), &ViewState::default(), &context(), 140, 20);
    let style = |time: &str| {
        lines
            .iter()
            .find(|line| line.text.contains(time))
            .map(|line| line.style)
    };
    assert_eq!(style("14:32"), Some(Style::Errored));
    assert_eq!(style("14:34"), Some(Style::Local));
}

#[test]
fn names_numbers_and_times() {
    assert_eq!(arc_name(Some("arc-swift-1.0")), "ARC Swift");
    assert_eq!(arc_name(Some("arc-swift-pro-2")), "ARC Swift Pro");
    assert_eq!(arc_name(Some("other-model")), "other-model");
    assert_eq!(preset_name("arc-policy.prefer-local.v1"), "prefer local");
    assert_eq!(
        preset_name("arc-policy.package-default.v1"),
        "package default"
    );
    assert_eq!(
        preset_name("arc-policy.package-default.v12"),
        "arc-policy.package-default.v12"
    );
    assert_eq!(preset_name("my.preset"), "my.preset");
    assert_eq!(tokens(999), "999");
    assert_eq!(tokens(12_100), "12.1k");
    assert_eq!(tokens(999_949), "999.9k");
    assert_eq!(tokens(999_950), "1.0M");
    assert_eq!(duration(122), "122 ms");
    assert_eq!(duration(1_600), "1.6 s");
    assert_eq!(duration(12_340), "12 s");
    assert_eq!(duration(123_456), "123 s");
    assert_eq!(uptime(42_000), "42 s");
    assert_eq!(uptime(12 * 60_000 + 5_000), "12 min");
    assert_eq!(uptime(3 * 3_600_000 + 5 * 60_000), "3 h 5 min");
    assert_eq!(clock(NOW, 0, true), "14:35:20");
    assert_eq!(clock(NOW, -8 * 3600, false), "06:35");
    assert_eq!(thousands(20_940), "20,940");
    assert_eq!(fit("claude-haiku-5.5", 8), "claude-…");
    assert_eq!(fit("服务器服务器", 6), "服务…");
    assert_eq!(cell_count("服务…"), 5);
    assert_eq!(cell_count("e\u{301}"), 1);
    assert_eq!(fit("a\nb\x1b[2J", 20), "a\u{FFFD}b\u{FFFD}[2J");
    assert_eq!(cell_count("a\nb"), 3);
    // Emoji take two cells, as a terminal draws them.
    assert_eq!(cell_count("🚀x"), 3);
    assert_eq!(fit("🚀🚀🚀", 4), "🚀…");
    // A heart with its emoji presentation selector takes two cells.
    assert_eq!(cell_count("\u{2764}\u{fe0f}"), 2);
    assert_eq!(cell_count(&fit("\u{2764}\u{fe0f}\u{2764}\u{fe0f}x", 4)), 4);
}

/// Wide characters, such as an endpoint key in Chinese, take two cells
/// each: rows are measured and cut in cells, so they still never wrap.
#[test]
fn wide_names_are_measured_in_cells() {
    let endpoints = [(
        "qwen3.5-35b-chat".to_owned(),
        "服务器服务器服务器服务器".to_owned(),
    )]
    .into_iter()
    .collect();
    let context = Context {
        endpoints: &endpoints,
        ..context()
    };
    let view = ViewState {
        selected: Some("req_1a1ea7bf3".to_owned()),
        ..ViewState::default()
    };
    for width in [80, 100, 140] {
        let lines = frame(Some(&snapshot()), &view, &context, &[], width, 18);
        assert!(lines.iter().any(|line| line.text.contains("服务")));
        for line in lines {
            assert!(cell_count(&line.text) <= width, "{width}: {}", line.text);
        }
    }
}

#[test]
fn text_output_has_the_header_and_every_row() {
    let text = text(&snapshot(), &context(), 140);
    assert!(text.starts_with("Rayline top"), "{text}");
    assert!(text.contains("Routing   ARC Swift decides on this Mac · policy: prefer local"));
    assert!(text.contains("Tokens    35.2k in (72% cached) · 1.4k out"));
    for time in ["14:35", "14:34", "14:33", "14:32"] {
        assert!(text.contains(time), "{text}");
    }
    assert!(
        !text.contains("req_"),
        "request ids only in details: {text}"
    );
}

#[test]
fn a_snapshot_without_the_new_fields_still_draws() {
    let old = json!({
        "totals": {"completed_requests": 1, "errored_requests": 0, "local_requests": 0,
            "cloud_requests": 1, "input_tokens": 10, "output_tokens": 2},
        "active": [],
        "recent": [{"request_id": "r", "started_at_unix_ms": NOW, "completed_at_unix_ms": NOW}]
    });
    let lines = frame(Some(&old), &ViewState::default(), &context(), &[], 80, 12);
    assert_eq!(lines[1].text, "Routing   —");
    assert_eq!(lines[3].text, "Tokens    10 in · 2 out");
    assert_eq!(lines.len(), 12);
}

/// An endpoint key with control characters, such as a newline or an escape
/// sequence, is shown with `�` in their place: no line carries a control
/// character to the terminal.
#[test]
fn control_characters_in_names_never_reach_the_terminal() {
    let endpoints = [("qwen3.5-35b-chat".to_owned(), "sp\nark\u{1b}[2J".to_owned())]
        .into_iter()
        .collect();
    let context = Context {
        endpoints: &endpoints,
        ..context()
    };
    for details in [false, true] {
        let view = ViewState {
            selected: Some("req_1a1ea7bf3".to_owned()),
            details,
            ..ViewState::default()
        };
        let lines = frame(Some(&snapshot()), &view, &context, &[], 140, 30);
        // The list names the local request's endpoint; the details show
        // the selected cloud request.
        if !details {
            assert!(lines.iter().any(|line| line.text.contains("sp\u{FFFD}ark")));
        }
        for line in lines {
            assert!(!line.text.chars().any(char::is_control), "{:?}", line.text);
            assert!(cell_count(&line.text) <= 140, "{:?}", line.text);
        }
    }
}

/// Each request's time is shown with the zone offset at that time: a
/// request from before a daylight-saving change keeps the offset it had.
#[test]
fn each_time_uses_the_offset_at_that_time() {
    let context = Context {
        // An hour ahead until 100 s ago, as if the clocks went back since.
        utc_offset_s: |unix_ms| if unix_ms < NOW - 100_000 { 3_600 } else { 0 },
        ..context()
    };
    let lines = frame(
        Some(&snapshot()),
        &ViewState::default(),
        &context,
        &[],
        140,
        18,
    );
    let rows: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
    // 200 s ago was 14:32 UTC, shown an hour ahead; 20 s ago keeps UTC.
    assert!(
        rows.iter().any(|row| row.starts_with(" 15:32")),
        "{rows:#?}"
    );
    assert!(
        rows.iter().any(|row| row.starts_with(" 14:35")),
        "{rows:#?}"
    );
}

/// With the list filtered to a harness, the decision count and median cover
/// that harness's requests only.
#[test]
fn a_filtered_header_counts_the_listed_decisions() {
    let all = header(&snapshot(), &context(), None, 140);
    let claude = header(&snapshot(), &context(), Some("claude"), 140);
    let decisions = |lines: &[Line]| {
        lines
            .iter()
            .find(|line| line.text.starts_with("Decisions"))
            .map(|line| line.text.clone())
            .unwrap()
    };
    assert_ne!(decisions(&all), decisions(&claude));
    let listed = list_rows(&snapshot(), Some("claude"))
        .iter()
        .filter(|row| str_field(row, "router").is_some())
        .count();
    assert!(
        decisions(&claude).starts_with(&format!("Decisions {listed} ")),
        "{}",
        decisions(&claude)
    );
}
