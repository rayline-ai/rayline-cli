//! What `rayline top` draws (spec section 3.4 items 4-9): the header, the
//! request list with its why line, and the details view, as plain lines of
//! text with a style each. The live view and the text output both draw from
//! here; nothing in this module touches the terminal.

use serde_json::Value;

/// How a line is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Style {
    Plain,
    Title,
    Dim,
    ColumnNames,
    Local,
    Errored,
    Selected,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Line {
    pub text: String,
    pub style: Style,
}

impl Line {
    /// A line of `text`, with any control character shown as `�`: names
    /// from the config, such as endpoint keys, reach the terminal here.
    fn new(text: String, style: Style) -> Self {
        let text = if text.chars().any(char::is_control) {
            text.chars().map(printable).collect()
        } else {
            text
        };
        Self { text, style }
    }
}

/// What the viewer chose: the harness the list is filtered to, the selected
/// request and whether its details are open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ViewState {
    pub filter: Option<String>,
    pub selected: Option<String>,
    pub details: bool,
}

/// What a frame is drawn from besides the snapshot.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Context<'a> {
    pub now_unix_ms: u64,
    /// The local time zone's offset from UTC at a time, in seconds: a
    /// request from before a daylight-saving change keeps its own offset.
    pub utc_offset_s: fn(u64) -> i64,
    /// The running local process's `local.inference.models` has a model.
    pub local_models: bool,
    /// The `local.inference.endpoints` key of each local deployment, by the
    /// deployment id a row's `endpoint_id` holds.
    pub endpoints: &'a std::collections::BTreeMap<String, String>,
    /// macOS: a local decision is made on this Mac, else on this PC.
    pub on_mac: bool,
    /// Advances each frame; turns the in-flight spinner.
    pub tick: usize,
}

/// The rows of the list, newest first: requests in flight, then finished ones.
pub(crate) fn list_rows<'a>(snapshot: &'a Value, filter: Option<&str>) -> Vec<&'a Value> {
    let mut active: Vec<&Value> = rows(snapshot, "active").iter().collect();
    let mut recent: Vec<&Value> = rows(snapshot, "recent").iter().collect();
    for rows in [&mut active, &mut recent] {
        rows.sort_by_key(|row| std::cmp::Reverse(u64_field(row, "started_at_unix_ms")));
    }
    active
        .into_iter()
        .chain(recent)
        .filter(|row| filter.is_none_or(|harness| str_field(row, "harness") == Some(harness)))
        .collect()
}

/// The harnesses that have requests in the snapshot, in name order: the
/// filters `f` cycles through after all requests.
pub(crate) fn harnesses(snapshot: &Value) -> Vec<String> {
    let mut names: Vec<String> = ["active", "recent"]
        .iter()
        .flat_map(|key| rows(snapshot, key))
        .filter_map(|row| str_field(row, "harness").map(str::to_owned))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The filter after `current`: all requests, then each harness in turn.
pub(crate) fn next_filter(snapshot: &Value, current: Option<&str>) -> Option<String> {
    let names = harnesses(snapshot);
    match current {
        None => names.into_iter().next(),
        Some(current) => names
            .iter()
            .position(|name| name == current)
            .and_then(|index| names.get(index + 1).cloned()),
    }
}

/// The selected row's index in `rows`: the selected request, else the first.
pub(crate) fn selected_index(rows: &[&Value], selected: Option<&str>) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    Some(
        selected
            .and_then(|id| {
                rows.iter()
                    .position(|row| str_field(row, "request_id") == Some(id))
            })
            .unwrap_or(0),
    )
}

/// The five header lines (item 4).
/// `filter`: the harness the list shows, if any; the decision count and
/// median cover the listed requests.
pub(crate) fn header(
    snapshot: &Value,
    context: &Context,
    filter: Option<&str>,
    width: usize,
) -> Vec<Line> {
    let totals = &snapshot["totals"];
    let started = u64_field(snapshot, "started_at_unix_ms");
    let status = format!(
        "local process · up {} · {}",
        uptime(
            context
                .now_unix_ms
                .saturating_sub(started.unwrap_or(context.now_unix_ms))
        ),
        clock(
            context.now_unix_ms,
            (context.utc_offset_s)(context.now_unix_ms),
            false
        ),
    );
    let title = "Rayline top";
    let gap = width.saturating_sub(cell_count(title) + cell_count(&status));
    let mut lines = vec![Line::new(
        format!("{title}{}{status}", " ".repeat(gap.max(1))),
        Style::Title,
    )];
    lines.push(Line::new(routing_line(snapshot, context), Style::Plain));
    lines.push(Line::new(
        format!(
            "Ran on    Rayline Platform {} · your servers {}",
            total(totals, "cloud_requests"),
            total(totals, "local_requests"),
        ),
        Style::Plain,
    ));
    let input = total(totals, "input_tokens");
    let cached = match totals.get("cache_read_tokens").and_then(Value::as_u64) {
        Some(read) if input > 0 => format!(" ({}% cached)", percent(read as f64 / input as f64)),
        _ => String::new(),
    };
    lines.push(Line::new(
        format!(
            "Tokens    {} in{cached} · {} out",
            tokens(input),
            tokens(total(totals, "output_tokens")),
        ),
        Style::Plain,
    ));
    let listed = list_rows(snapshot, filter);
    let mut times: Vec<u64> = listed
        .iter()
        .filter(|row| str_field(row, "router").is_some())
        .filter_map(|row| u64_field(row, "decision_ms"))
        .collect();
    let decisions = listed
        .iter()
        .filter(|row| str_field(row, "router").is_some())
        .count();
    times.sort_unstable();
    let median = match times.len() {
        0 => String::new(),
        n if n % 2 == 1 => format!(" · median {}", duration(times[n / 2])),
        n => format!(
            " · median {}",
            duration((times[n / 2 - 1] + times[n / 2]) / 2)
        ),
    };
    lines.push(Line::new(
        format!(
            "Decisions {decisions}{median} · {} errors",
            total(totals, "errored_requests")
        ),
        Style::Plain,
    ));
    lines
        .into_iter()
        .map(|line| Line::new(fit(&line.text, width), line.style))
        .collect()
}

fn routing_line(snapshot: &Value, context: &Context) -> String {
    let router = &snapshot["router"];
    let name = arc_name(router.get("model").and_then(Value::as_str));
    let place = match router.get("kind").and_then(Value::as_str) {
        Some("local") => this_machine(context).to_owned(),
        Some("cloud") => "in Rayline Platform".to_owned(),
        _ => return "Routing   —".to_owned(),
    };
    let policy = router
        .get("preset")
        .and_then(Value::as_str)
        .map(|preset| format!(" · policy: {}", preset_name(preset)))
        .unwrap_or_default();
    format!("Routing   {name} decides {place}{policy}")
}

fn this_machine(context: &Context) -> &'static str {
    if context.on_mac {
        "on this Mac"
    } else {
        "on this PC"
    }
}

/// A decisions model as a product name: `arc-swift-1.0` → `ARC Swift`. A
/// model whose id doesn't start `arc-` shows its id.
pub(crate) fn arc_name(model: Option<&str>) -> String {
    let Some(model) = model else {
        return "ARC".to_owned();
    };
    let Some(rest) = model.strip_prefix("arc-") else {
        return model.to_owned();
    };
    let words: Vec<String> = rest
        .split('-')
        .take_while(|word| !word.starts_with(|c: char| c.is_ascii_digit()))
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        })
        .collect();
    if words.is_empty() {
        "ARC".to_owned()
    } else {
        format!("ARC {}", words.join(" "))
    }
}

/// A policy preset in words: `arc-policy.prefer-local.v1` → `prefer local`.
/// A preset Rayline doesn't know shows its id.
pub(crate) fn preset_name(preset: &str) -> String {
    match preset {
        "arc-policy.prefer-local.v1" => "prefer local".to_owned(),
        "arc-policy.package-default.v1" => "package default".to_owned(),
        "arc-policy.hybrid-default.v1" => "hybrid default".to_owned(),
        _ => preset.to_owned(),
    }
}

/// One column of the list.
struct Column {
    name: &'static str,
    width: usize,
    right: bool,
    /// Dropped from narrow terminals in this order, first `Some(0)` (item 9).
    drop_order: Option<usize>,
}

const MODEL_MIN: usize = 16;
const MODEL_MAX: usize = 32;

/// The list's columns, `MODEL` at its minimum width.
const COLUMNS: [Column; 11] = [
    col("TIME", 5, false, None),
    col("TOOL", 8, false, Some(4)),
    col("TURN", 11, false, None),
    col("DECIDED", 17, false, None),
    col("MODEL", MODEL_MIN, false, None),
    col("RAN ON", 9, false, None),
    col("IN", 6, true, Some(3)),
    col("CACHED", 6, true, Some(2)),
    // Wide enough for an in-flight row's spinner and time (`⠋ 9.9 s`,
    // `⠋ 999 s`) when it is the only result column left.
    col("OUT", 7, true, None),
    col("TTFT", 6, true, Some(1)),
    col("TOK/S", 5, true, Some(0)),
];

const fn col(name: &'static str, width: usize, right: bool, drop_order: Option<usize>) -> Column {
    Column {
        name,
        width,
        right,
        drop_order,
    }
}

/// The index of the first result column: an in-flight row shows its spinner
/// and elapsed time from here on.
const RESULTS_FROM: usize = 6;

/// The columns shown at `width`, and `MODEL`'s width.
fn layout(width: usize) -> (Vec<usize>, usize) {
    let mut shown: Vec<usize> = (0..COLUMNS.len()).collect();
    let used = |shown: &[usize]| -> usize {
        1 + shown
            .iter()
            .map(|index| COLUMNS[*index].width + 1)
            .sum::<usize>()
            - 1
    };
    for order in 0.. {
        if used(&shown) <= width {
            break;
        }
        let Some(position) = shown
            .iter()
            .position(|index| COLUMNS[*index].drop_order == Some(order))
        else {
            break;
        };
        shown.remove(position);
    }
    let model = (MODEL_MIN + width.saturating_sub(used(&shown))).min(MODEL_MAX);
    (shown, model)
}

/// The list: the column names, then a line per row, with the selected row's
/// why line under it, in `height` lines at most. The rows shown start where
/// the selected row stays in view.
pub(crate) fn list(
    snapshot: &Value,
    view: &ViewState,
    context: &Context,
    width: usize,
    height: usize,
) -> Vec<Line> {
    let (shown, model_width) = layout(width);
    let cells: Vec<String> = COLUMNS
        .iter()
        .map(|column| column.name.to_owned())
        .collect();
    let mut lines = vec![Line::new(
        fit(&row_text(&shown, model_width, &cells, None), width),
        Style::ColumnNames,
    )];
    let rows = list_rows(snapshot, view.filter.as_deref());
    if rows.is_empty() {
        lines.push(Line::new(" no requests yet".to_owned(), Style::Dim));
        return lines;
    }
    let selected = selected_index(&rows, view.selected.as_deref());
    let why = selected.and_then(|index| why(rows[index], context));
    // Room for rows below the column names, less the why line.
    let room = height
        .saturating_sub(1)
        .saturating_sub(usize::from(why.is_some()))
        .max(1);
    let first = selected.map_or(0, |index| (index + 1).saturating_sub(room));
    for (index, row) in rows.iter().enumerate().skip(first).take(room) {
        let text = fit(
            &row_text(
                &shown,
                model_width,
                &row_cells(row, context),
                in_flight(row, context),
            ),
            width,
        );
        let style = if Some(index) == selected {
            Style::Selected
        } else if str_field(row, "state") == Some("error") {
            Style::Errored
        } else if str_field(row, "target") == Some("local") {
            Style::Local
        } else {
            Style::Plain
        };
        lines.push(Line::new(text, style));
        if Some(index) == selected
            && let Some(why) = &why
        {
            lines.push(Line::new(
                fit(&format!("   ▸ why: {why}"), width),
                Style::Dim,
            ));
        }
    }
    lines
}

/// An in-flight row's spinner and elapsed time.
fn in_flight(row: &Value, context: &Context) -> Option<String> {
    if row
        .get("completed_at_unix_ms")
        .is_some_and(|value| !value.is_null())
    {
        return None;
    }
    const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let elapsed = u64_field(row, "duration_ms").unwrap_or(0);
    Some(format!(
        "{} {}",
        SPINNER[context.tick % SPINNER.len()],
        duration(elapsed)
    ))
}

fn row_text(
    shown: &[usize],
    model_width: usize,
    cells: &[String],
    in_flight: Option<String>,
) -> String {
    let mut text = String::from(" ");
    let mut results = in_flight;
    // The column names line passes the names as `cells`.
    for (position, index) in shown.iter().enumerate() {
        let column = &COLUMNS[*index];
        let width = if column.name == "MODEL" {
            model_width
        } else {
            column.width
        };
        if position > 0 {
            text.push(' ');
        }
        if *index >= RESULTS_FROM
            && let Some(progress) = results.take()
        {
            let rest: usize = shown[position..]
                .iter()
                .map(|index| COLUMNS[*index].width + 1)
                .sum::<usize>()
                - 1;
            // The progress may run past the result columns into the line's end.
            text.push_str(&pad(&progress, rest.max(cell_count(&progress)), false));
            return text;
        }
        text.push_str(&pad(&cells[*index], width, column.right));
    }
    text
}

fn row_cells(row: &Value, context: &Context) -> Vec<String> {
    let started = u64_field(row, "started_at_unix_ms").unwrap_or(0);
    vec![
        clock(started, (context.utc_offset_s)(started), false),
        str_field(row, "harness").unwrap_or("—").to_owned(),
        str_field(row, "decision_event")
            .map(|event| event.replace('_', " "))
            .unwrap_or_else(|| "—".to_owned()),
        decided(row, context),
        str_field(row, "executed_model").unwrap_or("").to_owned(),
        ran_on(row, context),
        u64_field(row, "input_tokens")
            .map(tokens)
            .unwrap_or_default(),
        f64_field(row, "cache_hit_ratio")
            .map(|ratio| format!("{}%", percent(ratio)))
            .unwrap_or_default(),
        u64_field(row, "output_tokens")
            .map(tokens)
            .unwrap_or_default(),
        u64_field(row, "ttft_ms").map(duration).unwrap_or_default(),
        f64_field(row, "output_tps")
            .map(|rate| format!("{rate:.0}"))
            .unwrap_or_default(),
    ]
}

/// The `DECIDED` cell (item 5).
fn decided(row: &Value, context: &Context) -> String {
    let place = match str_field(row, "router") {
        Some("local") => {
            if context.on_mac {
                "Mac"
            } else {
                "this PC"
            }
        }
        Some("cloud") => "Platform",
        _ => return "—".to_owned(),
    };
    match u64_field(row, "decision_ms") {
        Some(ms) => format!("{place} {} →", duration(ms)),
        None => format!("{place} →"),
    }
}

fn ran_on(row: &Value, context: &Context) -> String {
    match str_field(row, "target") {
        Some("cloud") => "Platform".to_owned(),
        Some("local") => endpoint(row, context).unwrap_or("").to_owned(),
        _ => String::new(),
    }
}

/// The `local.inference.endpoints` key of the server a local row ran on.
fn endpoint<'a>(row: &Value, context: &Context<'a>) -> Option<&'a str> {
    str_field(row, "endpoint_id")
        .and_then(|id| context.endpoints.get(id))
        .map(String::as_str)
}

/// The decision's reason in words (item 6), or `None` when it gives none.
pub(crate) fn why(row: &Value, context: &Context) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(reason) = reason(row, context) {
        parts.push(reason);
    }
    if let Some(level) = str_field(row, "thinking_level").filter(|level| *level != "none") {
        parts.push(format!("thinking {level}"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn reason(row: &Value, context: &Context) -> Option<String> {
    match str_field(row, "decision_reason")? {
        "prefer_local_threshold" => Some(
            match (f64_field(row, "p_pass"), f64_field(row, "min_p_pass")) {
                (Some(p), Some(min)) => {
                    format!("local model good enough (pass {} ≥ {})", pass(p), pass(min))
                }
                _ => "local model good enough".to_owned(),
            },
        ),
        "utility_fallback" if context.local_models => Some("best value for this task".to_owned()),
        "utility_fallback" => {
            Some("best value for this task · no local model configured".to_owned())
        }
        "schedule" => Some("scheduled".to_owned()),
        _ => None,
    }
}

fn pass(value: f64) -> String {
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The details view of the selected request (item 8).
pub(crate) fn details(snapshot: &Value, row: &Value, context: &Context, width: usize) -> Vec<Line> {
    let started = u64_field(row, "started_at_unix_ms").unwrap_or(0);
    let mut title = vec![clock(started, (context.utc_offset_s)(started), true)];
    let mut what = Vec::new();
    if let Some(harness) = str_field(row, "harness") {
        what.push(harness.to_owned());
    }
    if let Some(event) = str_field(row, "decision_event") {
        what.push(event.replace('_', " "));
    }
    what.push(str_field(row, "request_id").unwrap_or("").to_owned());
    title.push(what.join(" · "));

    let arc = arc_name(snapshot["router"].get("model").and_then(Value::as_str));
    let took = u64_field(row, "decision_ms")
        .map(|ms| format!(" in {}", duration(ms)))
        .unwrap_or_default();
    let decided = match str_field(row, "router") {
        Some("local") => {
            let reuse = match (
                u64_field(row, "arc_restored_tokens"),
                u64_field(row, "arc_encoded_tokens"),
            ) {
                (Some(restored), Some(encoded)) => format!(
                    " ({} of {} prompt tokens reused)",
                    thousands(restored),
                    thousands(encoded)
                ),
                (None, Some(encoded)) => {
                    format!(" (0 of {} prompt tokens reused)", thousands(encoded))
                }
                _ => String::new(),
            };
            format!("{arc} {}{took}{reuse}", this_machine(context))
        }
        Some("cloud") => format!("{arc} in Rayline Platform{took}"),
        _ => "—".to_owned(),
    };

    let mut picked = vec![str_field(row, "decision_model").unwrap_or("—").to_owned()];
    if let Some(level) = str_field(row, "thinking_level") {
        picked.push(format!("thinking {level}"));
    }
    if let Some(reason) = reason(row, context) {
        picked.push(format!("reason: {reason}"));
    }

    let mut ran = vec![match str_field(row, "target") {
        Some("cloud") => "Rayline Platform".to_owned(),
        Some("local") => endpoint(row, context).unwrap_or("—").to_owned(),
        _ => "—".to_owned(),
    }];
    if let Some(ttft) = u64_field(row, "ttft_ms") {
        ran.push(format!("TTFT {}", duration(ttft)));
    }
    match (
        u64_field(row, "output_tokens"),
        f64_field(row, "output_tps"),
    ) {
        (Some(out), Some(rate)) => ran.push(format!("{} tokens at {rate:.0} tok/s", tokens(out))),
        (Some(out), None) => ran.push(format!("{} tokens", tokens(out))),
        _ => {}
    }

    let mut input = Vec::new();
    if let Some(total) = u64_field(row, "input_tokens") {
        input.push(tokens(total));
        let read = u64_field(row, "prompt_cache_tokens");
        let write = u64_field(row, "cache_write_tokens");
        if let Some(read) = read {
            input.push(format!("cache read {}", tokens(read)));
        }
        if let Some(write) = write {
            input.push(format!("cache write {}", tokens(write)));
        }
        if read.is_some() || write.is_some() {
            let uncached = total
                .saturating_sub(read.unwrap_or(0))
                .saturating_sub(write.unwrap_or(0));
            input.push(format!("uncached {}", tokens(uncached)));
        }
    }

    [
        format!(" {}", title.join("  ")),
        format!(" Decided   {decided}"),
        format!(" Picked    {}", picked.join(" · ")),
        format!(" Ran on    {}", ran.join(" · ")),
        format!(
            " Input     {}",
            if input.is_empty() {
                "—".to_owned()
            } else {
                input.join(" · ")
            }
        ),
    ]
    .into_iter()
    .map(|text| Line::new(fit(&text, width), Style::Plain))
    .collect()
}

/// The footer's keys (item 7).
pub(crate) fn footer(view: &ViewState, width: usize) -> Line {
    let text = if view.details {
        " esc back · q quit".to_owned()
    } else {
        format!(
            " ↑↓ select · enter details · f filter: {} · q quit",
            view.filter.as_deref().unwrap_or("all")
        )
    };
    Line::new(fit(&text, width), Style::Dim)
}

/// A whole frame of `width` × `height`: header, list or details, footer.
/// `warnings` (not running, a fetch error) go under the header.
pub(crate) fn frame(
    snapshot: Option<&Value>,
    view: &ViewState,
    context: &Context,
    warnings: &[String],
    width: usize,
    height: usize,
) -> Vec<Line> {
    let mut lines = match snapshot {
        Some(snapshot) => header(snapshot, context, view.filter.as_deref(), width),
        None => vec![Line::new(fit("Rayline top", width), Style::Title)],
    };
    for warning in warnings {
        lines.push(Line::new(fit(warning, width), Style::Warning));
    }
    lines.push(Line::new(String::new(), Style::Plain));
    if let Some(snapshot) = snapshot {
        let rows = list_rows(snapshot, view.filter.as_deref());
        let open = view
            .details
            .then(|| selected_index(&rows, view.selected.as_deref()))
            .flatten();
        match open {
            Some(index) => lines.extend(details(snapshot, rows[index], context, width)),
            None => {
                let room = height.saturating_sub(lines.len() + 1);
                lines.extend(list(snapshot, view, context, width, room));
            }
        }
    }
    lines.truncate(height.saturating_sub(1));
    while lines.len() < height.saturating_sub(1) {
        lines.push(Line::new(String::new(), Style::Plain));
    }
    lines.push(footer(view, width));
    lines
}

/// `rayline top` into a file or pipe: the header and every row, at `width`.
pub(crate) fn text(snapshot: &Value, context: &Context, width: usize) -> String {
    let mut lines = header(snapshot, context, None, width);
    lines.push(Line::new(String::new(), Style::Plain));
    let view = ViewState::default();
    let (shown, model_width) = layout(width);
    let names: Vec<String> = COLUMNS
        .iter()
        .map(|column| column.name.to_owned())
        .collect();
    lines.push(Line::new(
        fit(&row_text(&shown, model_width, &names, None), width),
        Style::ColumnNames,
    ));
    let rows = list_rows(snapshot, view.filter.as_deref());
    if rows.is_empty() {
        lines.push(Line::new(" no requests yet".to_owned(), Style::Dim));
    }
    for row in rows {
        lines.push(Line::new(
            fit(
                &row_text(
                    &shown,
                    model_width,
                    &row_cells(row, context),
                    in_flight(row, context),
                ),
                width,
            ),
            Style::Plain,
        ));
    }
    let mut output = String::new();
    for line in lines {
        output.push_str(line.text.trim_end());
        output.push('\n');
    }
    output
}

fn rows<'a>(snapshot: &'a Value, key: &str) -> &'a [Value] {
    snapshot
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn total(totals: &Value, key: &str) -> u64 {
    totals.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn str_field<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn u64_field(row: &Value, key: &str) -> Option<u64> {
    row.get(key).and_then(Value::as_u64)
}

fn f64_field(row: &Value, key: &str) -> Option<f64> {
    row.get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

/// `HH:MM`, or `HH:MM:SS` with `seconds`, in the local time zone.
pub(crate) fn clock(unix_ms: u64, utc_offset_s: i64, seconds: bool) -> String {
    let local = (unix_ms / 1000) as i64 + utc_offset_s;
    let of_day = local.rem_euclid(86_400);
    let (hours, minutes, secs) = (of_day / 3600, of_day / 60 % 60, of_day % 60);
    if seconds {
        format!("{hours:02}:{minutes:02}:{secs:02}")
    } else {
        format!("{hours:02}:{minutes:02}")
    }
}

/// How long the local process has run: `42 s`, `12 min`, `3 h 5 min`,
/// `2 d 4 h`.
pub(crate) fn uptime(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min", secs / 60),
        3600..86_400 => format!("{} h {} min", secs / 3600, secs / 60 % 60),
        _ => format!("{} d {} h", secs / 86_400, secs / 3600 % 24),
    }
}

/// A time: `122 ms` under a second, else seconds: `1.6 s`, and `12 s` from
/// 10 s (item 5).
pub(crate) fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 10_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{} s", ms / 1000)
    }
}

/// A token count: as is under 1,000, else `12.1k`, `3.4M` (item 5).
pub(crate) fn tokens(count: u64) -> String {
    match count {
        0..1000 => count.to_string(),
        1000..999_950 => format!("{:.1}k", count as f64 / 1e3),
        _ => format!("{:.1}M", count as f64 / 1e6),
    }
}

fn percent(ratio: f64) -> String {
    format!("{:.0}", (ratio * 100.0).clamp(0.0, 100.0))
}

/// `20940` → `20,940`.
fn thousands(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// `text` padded to `width`, or cut to it with `…`.
fn pad(text: &str, width: usize, right: bool) -> String {
    let cut = fit(text, width);
    let fill = " ".repeat(width.saturating_sub(cell_count(&cut)));
    if right {
        format!("{fill}{cut}")
    } else {
        format!("{cut}{fill}")
    }
}

/// `text` cut to `width` terminal cells, ending `…` when cut. Rows never
/// wrap.
pub(crate) fn fit(text: &str, width: usize) -> String {
    let shown: String = text.chars().map(printable).collect();
    if cell_count(&shown) <= width {
        return shown;
    }
    if width == 0 {
        return String::new();
    }
    // Widths are measured on the whole prefix, so a sequence such as an
    // emoji with its presentation selector counts as the terminal draws it.
    let mut cut = String::new();
    for c in shown.chars() {
        cut.push(c);
        if cell_count(&cut) > width - 1 {
            cut.pop();
            break;
        }
    }
    cut.push('…');
    cut
}

/// The terminal cells `text` takes, with control characters as [`printable`]
/// shows them.
pub(crate) fn cell_count(text: &str) -> usize {
    let shown: String = text.chars().map(printable).collect();
    unicode_width::UnicodeWidthStr::width(shown.as_str())
}

/// `c`, or `�` for a control character, which a terminal would act on.
fn printable(c: char) -> char {
    if c.is_control() { '\u{FFFD}' } else { c }
}

#[cfg(test)]
#[path = "top_view_tests.rs"]
mod tests;
