//! The encoder's prefix snapshots against full encodes: rows and decisions,
//! on synthetic inputs only. Run by `prefix_snapshots_read_and_decide_as_a_full_encode`.

use super::*;
use rayline_arc_decide::{DecideInput, ReadoutRequest, Readouts};
use std::sync::Mutex;

/// A row may differ from a full encode's by at most this, relative (L2).
/// The append path every later turn takes already differs by ~3e-3 once
/// another conversation holds cells before it (attention reads the cache in
/// another layout); continuing a snapshot is the same operation, held to the
/// same order. Decisions must not change at all.
const MAX_RELATIVE_L2: f64 = 1e-2;

const CONTEXT: u32 = 65_536;

fn files() -> EncoderFiles {
    let var = |name| PathBuf::from(std::env::var_os(name).expect(name));
    EncoderFiles {
        binary: var("ARC_ENCODER_BIN"),
        model: var("ARC_ENCODER_MODEL"),
    }
}

async fn encoder(context: u32) -> Encoder {
    Encoder::start_at(
        &files(),
        DEFAULT_BUDGET_TOKENS,
        Duration::from_secs(120),
        context,
    )
    .await
    .expect("the encoder starts")
}

/// Synthetic token ids, distinct per `seed`.
fn tokens(seed: u32, n: usize) -> Vec<u32> {
    (0..n as u32)
        .map(|i| 1_000 + (seed * 7_919 + i * 31) % 90_000)
        .collect()
}

/// An encoder input: blocks' token ids end to end, read at each block's end.
struct Input {
    tokens: Vec<u32>,
    offsets: Vec<usize>,
}

impl Input {
    fn new(blocks: &[Vec<u32>]) -> Self {
        let mut input = Self {
            tokens: Vec::new(),
            offsets: Vec::new(),
        };
        for block in blocks {
            input.push(block);
        }
        input
    }

    fn push(&mut self, block: &[u32]) {
        self.tokens.extend_from_slice(block);
        self.offsets.push(self.tokens.len() - 1);
    }
}

/// How many tokens the last encode took from a snapshot.
fn restored(client: &rayline_arc_decide::LocalEncoder) -> usize {
    client
        .take_stats()
        .last()
        .and_then(|stats| stats.restored_tokens)
        .unwrap_or(0)
}

/// `input`'s rows as `episode`, and how many tokens came from a snapshot.
fn read(
    client: &rayline_arc_decide::LocalEncoder,
    episode: &str,
    input: &Input,
) -> (Vec<Vec<f32>>, usize) {
    let rows = tokio::task::block_in_place(|| {
        client.readouts(&ReadoutRequest {
            episode: Some(episode),
            tokens: &input.tokens,
            offsets: &input.offsets,
        })
    })
    .expect("rows");
    (rows, restored(client))
}

fn relative_l2(a: &[f32], b: &[f32]) -> f64 {
    let diff: f64 = a.iter().zip(b).map(|(x, y)| f64::from(x - y).powi(2)).sum();
    let norm: f64 = b.iter().map(|y| f64::from(*y).powi(2)).sum();
    (diff / norm).sqrt()
}

/// The largest relative L2 between matching rows. A row that isn't finite
/// fails.
fn worst(a: &[Vec<f32>], b: &[Vec<f32>]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let error = relative_l2(x, y);
            assert!(error.is_finite(), "a row's error is {error}");
            error
        })
        .fold(0.0, f64::max)
}

/// `input`'s rows from a full encode on a fresh encoder.
async fn full(input: &Input) -> Vec<Vec<f32>> {
    let fresh = encoder(CONTEXT).await;
    let client = fresh.client().with_stats();
    let (rows, restored) = read(&client, "full", input);
    assert_eq!(restored, 0);
    rows
}

/// Checks `rows` against a full encode of `input`.
async fn check(label: &str, rows: &[Vec<f32>], restored: usize, input: &Input) {
    let error = worst(rows, &full(input).await);
    println!("{label}: {restored} tokens restored, worst relative L2 {error:.3e}");
    assert!(error <= MAX_RELATIVE_L2, "{label}: {error:e}");
}

/// Readouts that record what they return.
struct Recording<'a> {
    client: &'a rayline_arc_decide::LocalEncoder,
    rows: Mutex<Vec<Vec<f32>>>,
    restored: Mutex<usize>,
}

impl Readouts for Recording<'_> {
    fn readouts(&self, request: &ReadoutRequest<'_>) -> Result<Vec<Vec<f32>>, String> {
        let rows = self.client.readouts(request)?;
        *self.restored.lock().expect("lock") = restored(self.client);
        self.rows.lock().expect("lock").extend(rows.iter().cloned());
        Ok(rows)
    }
}

/// A synthetic Anthropic request: `tools` tool definitions and a system
/// prompt of `system` words, then `user`'s message.
fn request(tools: usize, system: usize, user: &str) -> String {
    let words = |seed: usize, n: usize| -> String {
        (0..n)
            .map(|i| format!("word{}", (seed * 131 + i * 17) % 997))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let tools: Vec<serde_json::Value> = (0..tools)
        .map(|i| {
            serde_json::json!({
                "name": format!("tool_{i}"),
                "description": words(i, 300),
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
            })
        })
        .collect();
    serde_json::json!({
        "model": "arc-swift-1.0",
        "max_tokens": 1024,
        "system": [{"type": "text", "text": words(10_000, system)}],
        "tools": tools,
        "messages": [{"role": "user", "content": user}],
    })
    .to_string()
}

fn decide_input(
    release: &rayline_arc_decide::Release,
    episode: &str,
    preset: &str,
    request: &str,
) -> DecideInput {
    let ids: Vec<&str> = release.actions.iter().map(|a| a.id.as_str()).collect();
    let body = format!(
        r#"{{"episode":{},"request":{request},"selection":{{"available_action_ids":{}}},"policy":{{"preset":{}}},"event":{{"kind":"user_message","basis":"marker_inferred"}}}}"#,
        serde_json::json!(episode),
        serde_json::json!(ids),
        serde_json::json!(preset),
    );
    serde_json::from_str(&body).expect("a decide input")
}

/// `value` with its non-integer numbers taken out into `floats`, by path.
fn without_floats(
    value: &serde_json::Value,
    path: &str,
    floats: &mut Vec<(String, f64)>,
) -> serde_json::Value {
    match value {
        serde_json::Value::Number(n) if n.is_f64() => {
            floats.push((path.to_owned(), n.as_f64().unwrap_or(f64::NAN)));
            serde_json::Value::Null
        }
        serde_json::Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| without_floats(v, &format!("{path}[{i}]"), floats))
            .collect(),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| (k.clone(), without_floats(v, &format!("{path}.{k}"), floats)))
            .collect(),
        other => other.clone(),
    }
}

/// A decision: its shape without floats, the floats, the rows it read and
/// how many tokens of them came from a snapshot.
struct Decision {
    shape: serde_json::Value,
    floats: Vec<(String, f64)>,
    rows: Vec<Vec<f32>>,
    restored: usize,
}

fn decide(
    release: &rayline_arc_decide::Release,
    client: &rayline_arc_decide::LocalEncoder,
    input: &DecideInput,
) -> Decision {
    let recording = Recording {
        client,
        rows: Mutex::default(),
        restored: Mutex::default(),
    };
    let output =
        tokio::task::block_in_place(|| rayline_arc_decide::decide(release, input, &recording))
            .expect("a decision");
    let mut floats = Vec::new();
    let shape = without_floats(
        &serde_json::to_value(output).expect("json"),
        "",
        &mut floats,
    );
    Decision {
        shape,
        floats,
        rows: recording.rows.into_inner().expect("lock"),
        restored: recording.restored.into_inner().expect("lock"),
    }
}

pub async fn run() {
    // Exact sizes at the encode level: a 6k-token first block, a 4k-token
    // second, then a message. Snapshots are kept at 6,000 and 10,000.
    let prefix = [tokens(1, 6_000), tokens(2, 4_000)];
    let conversation = |seed: u32, tail: usize| {
        Input::new(&[prefix[0].clone(), prefix[1].clone(), tokens(seed, tail)])
    };

    let main = encoder(CONTEXT).await;
    let client = main.client().with_stats();
    let mut a = conversation(10, 1_000);
    assert_eq!(read(&client, "a", &a).1, 0);
    // The snapshot's source goes on decoding two more turns.
    for turn in 0..2u32 {
        a.push(&tokens(20 + turn, 3_000));
        let (rows, restored) = read(&client, "a", &a);
        assert_eq!(restored, 0, "an append restores nothing");
        if turn == 1 {
            check(
                "append, no other conversation held (baseline)",
                &rows,
                0,
                &a,
            )
            .await;
        }
    }
    let b = conversation(11, 1_000);
    let (rows, restored) = read(&client, "b", &b);
    assert_eq!(
        restored, 10_000,
        "b continues the snapshot before its last block"
    );
    check(
        "fork after its source appended two turns",
        &rows,
        restored,
        &b,
    )
    .await;
    // The append path with another conversation holding cells: what every
    // later turn of a running encoder does.
    let mut b_more = conversation(11, 1_000);
    b_more.push(&tokens(30, 2_000));
    let (rows, _) = read(&client, "b", &b_more);
    check(
        "append, another conversation held (baseline)",
        &rows,
        0,
        &b_more,
    )
    .await;

    // Other prefixes take every slot: the least recently used snapshots are
    // replaced, and a freed slot is taken again.
    for seed in 40..43u32 {
        let other = Input::new(&[
            tokens(seed, 2_000),
            tokens(seed + 100, 2_000),
            tokens(seed + 200, 500),
        ]);
        read(&client, &format!("other{seed}"), &other);
    }
    let c = conversation(12, 1_000);
    let (rows, restored) = read(&client, "c", &c);
    assert_eq!(restored, 0, "the snapshots of c's prefix were replaced");
    check("after the snapshots were replaced", &rows, restored, &c).await;
    let recent = Input::new(&[tokens(42, 2_000), tokens(142, 2_000), tokens(50, 700)]);
    let (rows, restored) = read(&client, "recent", &recent);
    assert_eq!(restored, 4_000, "a snapshot in a reused slot is continued");
    check(
        "fork of a snapshot in a reused slot",
        &rows,
        restored,
        &recent,
    )
    .await;
    drop(main);

    // A context of three chunks: the snapshot's source session is evicted
    // for room while the snapshot stays.
    let small = encoder(3 * DEFAULT_BUDGET_TOKENS).await;
    let client = small.client().with_stats();
    let mut a = conversation(10, 1_000);
    read(&client, "a", &a);
    a.push(&tokens(21, 6_000));
    read(&client, "a", &a);
    assert_eq!(read(&client, "b", &conversation(11, 1_000)).1, 10_000);
    read(
        &client,
        "crowd",
        &Input::new(&[tokens(60, 4_000), tokens(61, 4_000)]),
    );
    let d = conversation(13, 1_000);
    let (rows, restored) = read(&client, "d", &d);
    assert_eq!(restored, 10_000, "the snapshot outlives its source session");
    check(
        "fork after its source session was evicted",
        &rows,
        restored,
        &d,
    )
    .await;
    drop(small);

    // Decisions: a second conversation sharing the first's tools and system
    // prompt decides as a full encode of it does.
    let release = load_release(&PathBuf::from(
        std::env::var_os("ARC_RUNTIME_RELEASE").expect("ARC_RUNTIME_RELEASE"),
    ))
    .expect("the release loads");
    let first = request(30, 2_000, "Rename the config loader and update its tests.");
    let second = request(30, 2_000, "Why does the build fail on the release branch?");
    for preset in [
        "arc-policy.package-default.v1",
        "arc-policy.prefer-local.v1",
    ] {
        let main = encoder(CONTEXT).await;
        let client = main.client().with_stats();
        decide(
            &release,
            &client,
            &decide_input(&release, "one", preset, &first),
        );
        let reused = decide(
            &release,
            &client,
            &decide_input(&release, "two", preset, &second),
        );
        drop(main);
        let fresh = encoder(CONTEXT).await;
        let client = fresh.client().with_stats();
        let full = decide(
            &release,
            &client,
            &decide_input(&release, "two", preset, &second),
        );
        drop(fresh);
        assert!(
            reused.restored > 0,
            "{preset}: the second conversation continues a snapshot"
        );
        assert_eq!(full.restored, 0);
        assert_eq!(
            reused.shape, full.shape,
            "{preset}: the same pick and outcome"
        );
        let rows = worst(&reused.rows, &full.rows);
        let (path, score) = reused
            .floats
            .iter()
            .zip(&full.floats)
            .map(|((path, x), (_, y))| {
                let difference = (x - y).abs() / y.abs().max(1e-9);
                assert!(
                    difference.is_finite(),
                    "{preset}: {path} differs by {difference}"
                );
                (path.as_str(), difference)
            })
            .fold(("", 0.0), |a, b| if b.1 > a.1 { b } else { a });
        println!(
            "decision {preset}: {} tokens restored, rows worst relative L2 {rows:.3e}, \
             scores worst relative difference {score:.3e} ({path})",
            reused.restored
        );
        assert!(rows <= MAX_RELATIVE_L2, "{preset}: {rows:e}");
    }
}
