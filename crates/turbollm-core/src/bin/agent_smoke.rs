//! End-to-end smoke test of the agent loop with a real GGUF model.
//!
//! ```bash
//! cargo run --release --no-default-features --bin agent-smoke -- model.gguf react
//! cargo run --release --no-default-features --bin agent-smoke -- model.gguf auto
//! ```
//!
//! It is what CI should run to prove that tool calling works on a real model and
//! not only in unit tests.

use turbollm_core::api::{ChatMessage, GenParams};
use turbollm_core::engine::{Engine, KvType, LoadOptions};
use turbollm_core::session::ChatSession;
use turbollm_core::{agent, builtin_tools};

fn main() {
    let mut args = std::env::args().skip(1);
    let model_path = args
        .next()
        .expect("usage: agent-smoke <model.gguf> [react|auto]");
    let mode = args.next().unwrap_or_else(|| "react".to_string());

    let engine = Engine::load(
        &model_path,
        LoadOptions {
            n_ctx: 2048,
            kv_type: KvType::Q8_0,
            n_threads: 4,
            n_threads_batch: 4,
            ..Default::default()
        },
    )
    .expect("load");

    let mut session = ChatSession::new(
        "Ты — ассистент. Для вычислений всегда используй инструмент calc.",
        GenParams {
            max_tokens: 192,
            ..Default::default()
        },
    );

    let tools = builtin_tools::builtin_specs();
    let cfg = agent::AgentConfig {
        mode: if mode == "auto" {
            agent::AgentMode::Auto
        } else {
            agent::AgentMode::ReAct
        },
        max_steps: 4,
        max_consecutive_failures: 1,
        force_json: false,
        gen: GenParams {
            max_tokens: 192,
            ..Default::default()
        },
        ..Default::default()
    };

    let history = vec![ChatMessage::user(
        "Сколько будет (123 * 45) + 67? Используй инструмент calc и назови число.",
    )];

    let mut exec = |call: &turbollm_core::tools::ToolCall| -> Result<String, String> {
        println!("   [executor] {} {:?}", call.name, call.arguments);
        Err("на хосте доступны только встроенные инструменты".to_string())
    };

    let (answer, stats) = agent::run(
        &mut session,
        &engine,
        &history,
        &tools,
        &cfg,
        &mut exec,
        |ev| match ev {
            agent::AgentEvent::Token { text } => print!("{text}"),
            agent::AgentEvent::Thought { text, step } => {
                println!("\n💭 step {step}: {text}")
            }
            agent::AgentEvent::ToolCall { name, arguments } => {
                println!("\n🔧 {name} {arguments}")
            }
            agent::AgentEvent::ToolResult { name, ok, output } => {
                println!("\n{} {name} -> {output}", if ok { "✅" } else { "⚠" })
            }
            agent::AgentEvent::Done { .. } => println!("\n— done —"),
        },
    )
    .expect("agent run");

    println!("\n=== ANSWER ===\n{answer}");
    println!(
        "stats: {} prompt tok ({} cached), {} generated, {:.2} tok/s",
        stats.prompt_tokens, stats.cached_tokens, stats.generated_tokens, stats.decode_tps
    );
}
