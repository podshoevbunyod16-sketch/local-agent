//! Headless benchmark: same engine the Android app runs, no Android required.
//!
//! ```bash
//! cargo run --release --no-default-features --bin turbollm-bench -- \
//!     --model model.gguf --tokens 128 --turns 3
//! ```
//!
//! It measures exactly the three numbers that matter for perceived speed:
//!   * prefill tok/s (first turn, nothing cached)
//!   * prefill tok/s on turn 2..N (KV cache reuse → the "several times faster" effect)
//!   * decode tok/s (tokens/second while the answer is written)

use std::path::PathBuf;
use std::time::Instant;

use turbollm_core::api::{ChatMessage, GenParams};
use turbollm_core::engine::{Engine, KvType, LoadOptions};
use turbollm_core::session::ChatSession;

struct Args {
    model: PathBuf,
    tokens: i32,
    turns: usize,
    ctx: u32,
    threads: i32,
    kv: KvType,
    flash: bool,
    /// Also run the naive "re-evaluate the whole history" variant so the speedup
    /// can be reported as a ratio.
    compare: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        model: PathBuf::new(),
        tokens: 128,
        turns: 3,
        ctx: 4096,
        threads: LoadOptions::default().n_threads,
        kv: KvType::Q8_0,
        flash: true,
        compare: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().expect("missing value");
        match k.as_str() {
            "--model" | "-m" => a.model = PathBuf::from(val()),
            "--tokens" | "-n" => a.tokens = val().parse().unwrap(),
            "--turns" => a.turns = val().parse().unwrap(),
            "--ctx" | "-c" => a.ctx = val().parse().unwrap(),
            "--threads" | "-t" => a.threads = val().parse().unwrap(),
            "--kv" => {
                a.kv = match val().as_str() {
                    "f16" => KvType::F16,
                    "q4_0" => KvType::Q4_0,
                    _ => KvType::Q8_0,
                }
            }
            "--no-flash" => a.flash = false,
            "--compare" => a.compare = true,
            "--help" | "-h" => {
                println!("usage: turbollm-bench --model <gguf> [-n 128] [--turns 3] [-c 4096] [-t 4] [--kv q8_0|q4_0|f16] [--no-flash]");
                std::process::exit(0);
            }
            other => panic!("unknown arg {other}"),
        }
    }
    assert!(a.model.exists(), "model not found: {}", a.model.display());
    a
}

fn main() {
    let args = parse_args();
    let opts = LoadOptions {
        n_ctx: args.ctx,
        n_threads: args.threads,
        n_threads_batch: args.threads,
        kv_type: args.kv,
        flash_attn: args.flash,
        ..Default::default()
    };

    println!(
        "loading {} (threads={}, ctx={}, kv={:?}, flash={})",
        args.model.display(),
        args.threads,
        args.ctx,
        args.kv,
        args.flash
    );
    let t0 = Instant::now();
    let engine = Engine::load(&args.model, opts).expect("load failed");
    println!(
        "model loaded in {:.2}s: {:?}",
        t0.elapsed().as_secs_f64(),
        engine.info()
    );

    let params = GenParams {
        max_tokens: args.tokens,
        n_threads: args.threads,
        n_threads_batch: args.threads,
        temperature: 0.7,
        ..Default::default()
    };

    let mut history: Vec<ChatMessage> = Vec::new();
    let mut total_saved = 0u64;
    // One session for the whole conversation: this is what keeps the KV cache
    // warm between turns (the speed win we are measuring).
    let mut session = ChatSession::new("You are a concise assistant.", params.clone());
    let mut naive_ms: f64 = 0.0;
    let mut cached_ms: f64 = 0.0;
    // Histories of every turn, replayed later to compare against the naive mode.
    let mut snapshots: Vec<Vec<ChatMessage>> = Vec::new();

    for turn in 1..=args.turns {
        history.push(ChatMessage::user(format!(
            "Turn {turn}: explain in two sentences why KV-cache reuse makes chat faster."
        )));

        snapshots.push(history.clone());
        let t = Instant::now();
        let (text, stats) = session
            .generate(&engine, &history, |_| {})
            .expect("generate failed");
        history.push(ChatMessage::assistant(text.clone()));
        total_saved += stats.cached_tokens;
        cached_ms += stats.prompt_ms;

        println!(
            "turn {turn}: prompt {:>4} tok (cached {:>4}) in {:>7.1} ms ({:>6.1} tok/s) | \
             gen {:>4} tok in {:>7.1} ms = {:>6.2} tok/s | ttft {:.0} ms | stop {:?}",
            stats.prompt_tokens,
            stats.cached_tokens,
            stats.prompt_ms,
            stats.prefill_tps,
            stats.generated_tokens,
            stats.decode_ms,
            stats.decode_tps,
            stats.ttft_ms,
            stats.stop_reason
        );
        let _ = t;
    }
    println!("total prompt tokens skipped by the KV cache: {total_saved}");

    if args.compare {
        // ---- baseline: naive engine, whole history re-read on every turn ----
        let mut naive = ChatSession::new("You are a concise assistant.", params.clone());
        for h in &snapshots {
            naive.reset(&engine);
            if let Ok((_, s)) = naive.generate(&engine, h, |_| {}) {
                naive_ms += s.prompt_ms;
            }
        }
        // ---- ours: one warm session, only the new suffix is decoded ----
        session.reset(&engine);
        for h in &snapshots {
            if let Ok((_, s)) = session.generate(&engine, h, |_| {}) {
                cached_ms += s.prompt_ms;
            }
        }
        println!(
            "\nprefill of {} turns: naive {:.0} ms vs KV-reuse {:.0} ms  =>  {:.2}x faster \
             (time to first token drops the same way)",
            snapshots.len(),
            naive_ms,
            cached_ms,
            naive_ms / cached_ms.max(0.001)
        );
    }
}
