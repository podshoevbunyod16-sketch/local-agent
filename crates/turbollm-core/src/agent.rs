//! The agent loop: model → tool call → tool result → model, until it answers.
//!
//! Design notes that matter on a phone:
//!
//! * **Budgeted.** Each step re-ingests only the new part of the conversation
//!   (the KV cache is warm from the previous step), but every step still costs
//!   time, so `max_steps` defaults to 4 and every step has a token limit.
//! * **Robust to small models.** If the model emits a call to an unknown tool we
//!   feed the error back instead of failing — this recovers the conversation on
//!   a 0.5B model instead of dead-ending.
//! * **Every call is visible.** The UI gets `on_event` notifications for
//!   "thinking", "calling tool", "tool result", so the user sees what the agent
//!   is doing (and can stop it) — the same UX as a terminal agent.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{ChatMessage, GenParams, GenStats, Role};
use crate::builtin_tools::run_builtin;
use crate::engine::Engine;
use crate::error::{CoreError, Result};
use crate::session::ChatSession;
use crate::tools::{parse_tool_call, render_tools_prompt, ToolCall, ToolSpec, ToolStyle};

/// Everything that can happen during one agent run.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// The model is producing text (streamed, may be empty for pure tool calls).
    Token { text: String },
    /// ReAct: the reasoning paragraph of one step.
    Thought { text: String, step: u32 },
    /// The model decided to call a tool.
    ToolCall { name: String, arguments: Value },
    /// The tool finished; `ok` is false for errors the model can react to.
    ToolResult {
        name: String,
        ok: bool,
        output: String,
    },
    /// Final answer, no more tool calls.
    Done { text: String, stats: GenStats },
}

/// Loop shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// Model decides freely when to stop; tools optional (good for chat).
    #[default]
    Auto,
    /// Strict ReAct: every step must be `Thought: … Action: … Action Input: …`
    /// and the loop runs until `Final Answer:` or a step/error budget runs out.
    ReAct,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// `auto` (tool-calling protocol) or `react` (Thought/Action/Observation).
    #[serde(default)]
    pub mode: AgentMode,
    /// Hard limit on model↔tool round trips (ReAct: max Thought/Action cycles).
    pub max_steps: u32,
    /// Keep going after a failed tool call (ReAct re-plans), up to this many
    /// consecutive failures. This is what "работать до успеха" means in practice:
    /// the agent retries with a corrected call instead of dying on step 1.
    #[serde(default = "default_retries")]
    pub max_consecutive_failures: u32,
    /// Force the model output of a tool-calling step to be valid JSON (GBNF).
    pub force_json: bool,
    /// Truncate tool output before feeding it back (context is precious).
    pub max_observation_chars: usize,
    pub style: ToolStyle,
    pub gen: GenParams,
}

fn default_retries() -> u32 {
    2
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            mode: AgentMode::Auto,
            max_steps: 4,
            max_consecutive_failures: 2,
            force_json: false,
            max_observation_chars: 4_000,
            style: ToolStyle::Auto,
            gen: GenParams::default(),
        }
    }
}

/// How to execute a tool. The app supplies this closure; it handles HTTP tools,
/// Android APIs and MCP servers (the Rust core cannot own TLS/child processes).
pub type ToolExecutor<'a> = dyn FnMut(&ToolCall) -> std::result::Result<String, String> + 'a;

/// Run the agent loop.
///
/// * `history` — the conversation so far, **without** the new answer.
/// * `tools` — every tool the model may call (built-in, plugin and MCP).
/// * `execute` — runs a single call; returns the observation text.
/// * `on_event` — streamed back to the UI.
pub fn run(
    session: &mut ChatSession,
    engine: &Engine,
    history: &[ChatMessage],
    tools: &[ToolSpec],
    cfg: &AgentConfig,
    execute: &mut ToolExecutor<'_>,
    mut on_event: impl FnMut(AgentEvent),
) -> Result<(String, GenStats)> {
    if tools.is_empty() {
        let (text, stats) = session.generate(engine, history, |t| {
            on_event(AgentEvent::Token {
                text: t.to_string(),
            })
        })?;
        on_event(AgentEvent::Done {
            text: text.clone(),
            stats: stats.clone(),
        });
        return Ok((text, stats));
    }

    if cfg.mode == AgentMode::ReAct {
        return run_react(session, engine, history, tools, cfg, execute, on_event);
    }

    let style = crate::tools::detect_style(
        &engine.info().description,
        engine.chat_template().as_deref(),
        cfg.style,
    );
    let tools_prompt = render_tools_prompt(tools, style);
    session.set_tools_prompt(&tools_prompt);

    let mut working: Vec<ChatMessage> = history.to_vec();
    let mut total = GenStats::default();
    let mut last_text = String::new();

    for _step in 0..cfg.max_steps.max(1) {
        let mut step_text = String::new();
        let (text, stats) = {
            let mut params = cfg.gen.clone();
            if cfg.force_json {
                params.grammar = Some(crate::tools::tool_grammar(style));
            }
            session.set_params(params);
            session.generate(engine, &working, |t| {
                step_text.push_str(t);
                on_event(AgentEvent::Token {
                    text: t.to_string(),
                })
            })?
        };
        total.cached_tokens += stats.cached_tokens;
        total.prompt_tokens += stats.prompt_tokens;
        total.generated_tokens += stats.generated_tokens;
        total.prompt_ms += stats.prompt_ms;
        total.decode_ms += stats.decode_ms;
        total.prefill_tps = stats.prefill_tps;
        total.decode_tps = stats.decode_tps;
        total.ttft_ms = stats.ttft_ms;
        total.stop_reason = stats.stop_reason;
        last_text = text.clone();

        let Some(call) = parse_tool_call(&text, style) else {
            // A normal answer: we are done.
            on_event(AgentEvent::Done {
                text: text.clone(),
                stats: total.clone(),
            });
            return Ok((text, total));
        };

        on_event(AgentEvent::ToolCall {
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        });

        let result = execute_call(&call, tools, execute, cfg.max_observation_chars);

        let (ok, output) = match result {
            Ok(out) => (true, out),
            Err(e) => (false, e),
        };
        on_event(AgentEvent::ToolResult {
            name: call.name.clone(),
            ok,
            output: output.clone(),
        });

        // Feed the call and its observation back. Models expect their own
        // tool-call syntax in the history, so we keep the raw step text.
        working.push(ChatMessage {
            role: Role::Assistant,
            content: text.clone(),
        });
        working.push(ChatMessage::user(format!(
            "<tool_response>\n{}\n</tool_response>\n\n\
             Continue. Use another tool only if it is really needed, \
             otherwise give the final answer to the user.",
            truncate(&output, cfg.max_observation_chars)
        )));
    }

    // Step budget exhausted: still return something useful.
    if last_text.trim().is_empty() {
        return Err(CoreError::Invalid(
            "агент израсходовал лимит шагов без ответа".into(),
        ));
    }
    on_event(AgentEvent::Done {
        text: last_text.clone(),
        stats: total.clone(),
    });
    Ok((last_text, total))
}

/// Map what the model wrote onto a real tool name.
///
/// `Action: calc Action Input {...}` (newline lost) or `Action: calc(...)` are
/// common on small models; an unknown name is a recoverable error, but guessing
/// right saves a whole loop iteration.
pub fn resolve_tool_name(candidate: &str, tools: &[ToolSpec]) -> String {
    let cleaned = candidate
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim()
        .to_ascii_lowercase();
    if tools.iter().any(|t| t.name.eq_ignore_ascii_case(&cleaned)) {
        return tools
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(&cleaned))
            .map(|t| t.name.clone())
            .unwrap_or(cleaned);
    }
    // first token: "calc(" / "calc {...}" / "calculator"
    let first: String = cleaned
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if let Some(t) = tools.iter().find(|t| t.name.eq_ignore_ascii_case(&first)) {
        return t.name.clone();
    }
    // "calculator" -> "calc", "calc(x)" -> "calc"
    if let Some(t) = tools.iter().find(|t| {
        let name = t.name.to_ascii_lowercase();
        !first.is_empty() && (name.starts_with(&first) || first.starts_with(&name))
    }) {
        return t.name.clone();
    }
    cleaned
}

fn execute_call(
    call: &ToolCall,
    tools: &[ToolSpec],
    execute: &mut ToolExecutor<'_>,
    limit: usize,
) -> std::result::Result<String, String> {
    let known = tools.iter().any(|t| t.name == call.name);
    if !known {
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        return Err(format!(
            "инструмент «{}» не существует. Доступны: {}",
            call.name,
            names.join(", ")
        ));
    }
    if let Some(res) = run_builtin(&call.name, &call.arguments) {
        return res.map(|s| truncate(&s, limit));
    }
    execute(call).map(|s| truncate(&s, limit))
}

fn truncate(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let cut: String = s.chars().take(limit).collect();
    format!("{cut}\n…[обрезано, всего {} символов]", s.chars().count())
}

// ---------------------------------------------------------------------------
// ReAct: Thought → Action → Action Input → Observation → … → Final Answer
// ---------------------------------------------------------------------------

/// The ReAct system prompt. Kept close to the original paper / LangChain wording
/// because that is what instruct models were exposed to during fine-tuning.
pub fn react_tools_prompt(tools: &[ToolSpec]) -> String {
    let mut names = String::new();
    for t in tools {
        let schema = serde_json::to_string(&t.parameters).unwrap_or_default();
        names.push_str(&format!(
            "- {}: {} arguments (JSON schema: {})\n",
            t.name,
            if t.description.is_empty() {
                "без описания"
            } else {
                &t.description
            },
            schema
        ));
    }
    format!(
        "\n\nYou are a ReAct agent. Solve the task step by step, using tools when needed.\n\
         Available tools:\n{names}\n\
         Use exactly this format, one step at a time:\n\n\
         Thought: reason about what you know and what to do next\n\
         Action: one of [{list}]\n\
         Action Input: {{\"arg\": \"value\"}}\n\n\
         After the Action Input, stop. The tool result will be given to you as an \
         Observation. Then continue with a new Thought. When you know the final answer, \
         write it as:\n\n\
         Thought: I now know the final answer\n\
         Final Answer: <the answer for the user>\n\n\
         Rules: never invent an Observation, never call a tool that is not listed, \
         Action Input must be valid JSON on a single line.",
        list = tools
            .iter()
            .map(|t| t.name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReactStep {
    pub thought: String,
    pub action: Option<String>,
    pub action_input: serde_json::Value,
    pub final_answer: Option<String>,
}

/// Parse one ReAct block. Tolerant: models drop labels, add markdown, mix case.
pub fn parse_react(text: &str) -> ReactStep {
    let lower = text.to_ascii_lowercase();
    let thought = extract_labeled(text, &lower, "thought:", &["action:", "final answer:"]);
    // Small models write "Action: calc" but also "Action: calc\nAction Input {...}"
    // (no colon) — so the action is only the first line, and the stop-words do
    // not require a colon.
    let action = extract_labeled(
        text,
        &lower,
        "action:",
        &["action input", "final answer", "observation"],
    )
    .and_then(|a| {
        let first_line = a.lines().next().unwrap_or("").trim();
        let cleaned = first_line
            .trim_matches('`')
            .trim_matches('"')
            .trim_end_matches(':')
            .trim();
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned.to_string())
        }
    });
    // "Action Input:" and "Action Input" (colon optional) are both accepted.
    let input_raw = extract_labeled(
        text,
        &lower,
        "action input:",
        &["observation:", "thought:", "final answer:"],
    )
    .or_else(|| {
        extract_labeled(
            text,
            &lower,
            "action input",
            &["observation:", "thought:", "final answer:"],
        )
    })
    .unwrap_or_default();
    let action_input = parse_action_input(&input_raw);
    let final_answer =
        extract_labeled(text, &lower, "final answer:", &[]).map(|s| s.trim().to_string());
    ReactStep {
        thought: thought.unwrap_or_default().trim().to_string(),
        action,
        action_input,
        final_answer,
    }
}

fn extract_labeled(text: &str, lower: &str, label: &str, stops: &[&str]) -> Option<String> {
    let start = lower.find(label)? + label.len();
    let rest_lower = &lower[start..];
    let mut end = rest_lower.len();
    for stop in stops {
        if let Some(pos) = rest_lower.find(stop) {
            if pos < end {
                end = pos;
            }
        }
    }
    Some(text[start..start + end].to_string())
}

/// `{"a":1}`, `{a: 1}`, `{"a": 2*3}`, `{a: "1"}` и голый текст — всё принимается.
///
/// Small models write "almost JSON": unquoted keys and unquoted non-literal
/// values (`{expr: 2*3}`). Instead of regex-patching that, we split the object
/// into flat `key: value` pairs and parse each value on its own — which is
/// exactly the shape ReAct action inputs have.
fn parse_action_input(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim().trim_matches('`').trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return v;
    }

    let inner = trimmed.trim_start_matches('{').trim_end_matches('}').trim();
    let mut map = serde_json::Map::new();
    for pair in split_top_level(inner, ',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once(':') {
            Some((k, v)) => (k, v),
            None => continue,
        };
        let key = key.trim().trim_matches('"').trim_matches('\'').to_string();
        if key.is_empty() {
            continue;
        }
        let value_raw = value.trim();
        let value = serde_json::from_str::<serde_json::Value>(value_raw).unwrap_or_else(|_| {
            let unquoted = value_raw.trim_matches('"').trim_matches('\'').to_string();
            match serde_json::from_str::<serde_json::Value>(&unquoted) {
                Ok(v) => v,
                Err(_) => serde_json::Value::String(unquoted),
            }
        });
        map.insert(key, value);
    }
    if map.is_empty() {
        return serde_json::json!({ "input": trimmed });
    }
    serde_json::Value::Object(map)
}

/// Split on `sep` but only at the top level (ignores quotes and nested brackets).
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_str {
            current.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                current.push(c);
            }
            '{' | '[' => {
                depth += 1;
                current.push(c);
            }
            '}' | ']' => {
                depth -= 1;
                current.push(c);
            }
            c if c == sep && depth == 0 => {
                out.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// "До успеха": keep stepping until a Final Answer appears, the step budget is
/// exhausted, or a tool keeps failing the same way.
#[allow(clippy::too_many_arguments)]
fn run_react(
    session: &mut ChatSession,
    engine: &Engine,
    history: &[ChatMessage],
    tools: &[ToolSpec],
    cfg: &AgentConfig,
    execute: &mut ToolExecutor<'_>,
    mut on_event: impl FnMut(AgentEvent),
) -> Result<(String, GenStats)> {
    session.set_tools_prompt(&react_tools_prompt(tools));

    // The ReAct transcript is kept as a running scratchpad; the real user/assistant
    // history stays untouched so the UI stays clean.
    let mut scratchpad = String::new();
    let mut total = GenStats::default();
    let mut failures = 0u32;
    let mut last_answer = String::new();

    for step in 1..=cfg.max_steps.max(1) {
        // Prompt = conversation + scratchpad + the "next step" cue.
        let mut turn: Vec<ChatMessage> = history.to_vec();
        if !scratchpad.is_empty() {
            turn.push(ChatMessage::assistant(scratchpad.clone()));
        }
        turn.push(ChatMessage::user(
            "Continue. Output the next Thought/Action/Action Input, or the Final Answer."
                .to_string(),
        ));

        let mut params = cfg.gen.clone();
        // Stop at an observation boundary so a step never runs into a fake tool
        // result. The token budget is per step, not per answer.
        params.stop = vec!["\nObservation:".to_string(), "\nObservation :".to_string()];
        session.set_params(params);

        let (text, stats) = session.generate(engine, &turn, |t| {
            on_event(AgentEvent::Token {
                text: t.to_string(),
            })
        })?;
        total.cached_tokens += stats.cached_tokens;
        total.prompt_tokens += stats.prompt_tokens;
        total.generated_tokens += stats.generated_tokens;
        total.prompt_ms += stats.prompt_ms;
        total.decode_ms += stats.decode_ms;
        total.decode_tps = stats.decode_tps;
        total.prefill_tps = stats.prefill_tps;
        total.stop_reason = stats.stop_reason;

        let parsed = parse_react(&text);
        if !parsed.thought.is_empty() {
            on_event(AgentEvent::Thought {
                text: parsed.thought.clone(),
                step,
            });
        }
        scratchpad.push_str(&text);
        if !scratchpad.ends_with('\n') {
            scratchpad.push('\n');
        }

        // 1. Action wins over a premature Final Answer.
        //
        //    Small models routinely emit `Thought … Action: calc … Final Answer: 5508`
        //    in one block: answering *before* they ever saw the Observation. That
        //    is a hallucinated number. LangChain has the same rule — the loop only
        //    finishes on a block that has no Action.
        let action_name = match (parsed.action.clone(), parsed.final_answer.clone()) {
            (Some(_), Some(_)) => {
                scratchpad.push_str(
                    "Observation: ты не можешь дать Final Answer до того, как увидел \
                     результат инструмента. Сначала выполняется Action.\n",
                );
                parsed.action.clone()
            }
            (Some(a), None) => Some(a),
            (None, Some(answer)) if !answer.trim().is_empty() => {
                on_event(AgentEvent::Done {
                    text: answer.clone(),
                    stats: total.clone(),
                });
                return Ok((answer, total));
            }
            _ => None,
        };

        // 2. No action at all: treat the text as the answer if it looks final,
        //    otherwise nudge the model once (small models forget the format).
        let Some(action_name) = action_name else {
            if step == cfg.max_steps {
                last_answer = text.clone();
                break;
            }
            scratchpad.push_str(
                "Observation: формат не распознан. Ответь строго в формате \
                 Thought/Action/Action Input или Final Answer.\n",
            );
            failures += 1;
            if failures > cfg.max_consecutive_failures {
                last_answer = text.clone();
                break;
            }
            continue;
        };

        // 3. Action → tool → Observation.
        let action_name = resolve_tool_name(&action_name, tools);
        let call = ToolCall {
            name: action_name.clone(),
            arguments: parsed.action_input.clone(),
        };
        on_event(AgentEvent::ToolCall {
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        });
        let result = execute_call(&call, tools, execute, cfg.max_observation_chars);
        let (ok, output) = match result {
            Ok(out) => (true, out),
            Err(e) => (false, e),
        };
        on_event(AgentEvent::ToolResult {
            name: call.name.clone(),
            ok,
            output: output.clone(),
        });

        if ok {
            failures = 0;
        } else {
            failures += 1;
            if failures > cfg.max_consecutive_failures {
                // Give the model what we have and let it answer honestly.
                scratchpad.push_str(&format!(
                    "Observation: инструмент «{}» снова не сработал ({output}). \
                     Больше не вызывай его. Дай Final Answer и объясни, что не получилось.\n",
                    call.name
                ));
                last_answer = format!("Не удалось выполнить действие «{}»: {}", call.name, output);
                // one more pass so the model can phrase the answer itself
                if step < cfg.max_steps {
                    continue;
                }
                break;
            }
        }
        scratchpad.push_str(&format!("Observation: {output}\n"));
    }

    if last_answer.trim().is_empty() {
        return Err(CoreError::Invalid(
            "агент не пришёл к финальному ответу за отведённые шаги".into(),
        ));
    }
    on_event(AgentEvent::Done {
        text: last_answer.clone(),
        stats: total.clone(),
    });
    Ok((last_answer, total))
}

#[cfg(test)]
mod react_tests {
    use super::*;

    #[test]
    fn parses_full_step() {
        let text = "Thought: нужно посчитать\nAction: calc\nAction Input: {\"expr\": \"2+2\"}";
        let s = parse_react(text);
        assert_eq!(s.thought, "нужно посчитать");
        assert_eq!(s.action.as_deref(), Some("calc"));
        assert_eq!(s.action_input["expr"], "2+2");
        assert!(s.final_answer.is_none());
    }

    #[test]
    fn parses_final_answer() {
        let text = "Thought: я знаю ответ\nFinal Answer: 4";
        let s = parse_react(text);
        assert_eq!(s.final_answer.as_deref(), Some("4"));
    }

    #[test]
    fn tolerates_bare_keys_and_markdown() {
        let text = "Thought: x\nAction: `calc`\nAction Input: {expr: 2*3}";
        let s = parse_react(text);
        assert_eq!(s.action.as_deref(), Some("calc"));
        assert_eq!(s.action_input["expr"], "2*3");
    }

    #[test]
    fn stops_at_observation() {
        let text = "Thought: t\nAction: now\nAction Input: {}\nObservation: fake data";
        let s = parse_react(text);
        assert_eq!(s.action.as_deref(), Some("now"));
        assert_eq!(s.final_answer, None);
    }

    #[test]
    fn prompt_lists_tools() {
        let tools = crate::builtin_tools::builtin_specs();
        let p = react_tools_prompt(&tools);
        assert!(p.contains("calc"));
        assert!(p.contains("Final Answer:"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truncates_long_observations() {
        let s = "ы".repeat(10);
        assert_eq!(truncate(&s, 100), s);
        let t = truncate(&s, 4);
        assert!(t.starts_with("ыыыы"));
        assert!(t.contains("обрезано"));
    }

    #[test]
    fn unknown_tool_is_a_recoverable_error() {
        let tools = vec![ToolSpec {
            name: "calc".into(),
            description: String::new(),
            parameters: json!({"type":"object"}),
            kind: "local".into(),
            server: "builtin".into(),
        }];
        let mut exec = |_: &ToolCall| Ok("nope".to_string());
        let call = ToolCall {
            name: "weather".into(),
            arguments: json!({}),
        };
        let err = execute_call(&call, &tools, &mut exec, 100).unwrap_err();
        assert!(err.contains("не существует"));
        assert!(err.contains("calc"));
    }

    #[test]
    fn builtin_runs_without_executor() {
        let tools = vec![ToolSpec {
            name: "calc".into(),
            description: String::new(),
            parameters: json!({"type":"object"}),
            kind: "local".into(),
            server: "builtin".into(),
        }];
        let mut exec = |_: &ToolCall| panic!("executor must not be called for built-ins");
        let call = ToolCall {
            name: "calc".into(),
            arguments: json!({"expr": "3*4"}),
        };
        assert_eq!(execute_call(&call, &tools, &mut exec, 100).unwrap(), "12");
    }
}

#[cfg(test)]
mod react_flow_tests {
    use super::*;

    #[test]
    fn action_beats_premature_final_answer() {
        let text =
            "Thought: посчитаю\nAction: calc\nAction Input: {\"expr\": \"1+1\"}\nFinal Answer: 3";
        let s = parse_react(text);
        assert_eq!(s.action.as_deref(), Some("calc"));
        assert_eq!(s.final_answer.as_deref(), Some("3"));
        // The loop must prefer the action; the guard is: an action present means
        // "not finished yet".
        assert!(s.action.is_some());
        assert_eq!(
            super::resolve_tool_name("calc", &crate::builtin_tools::builtin_specs()),
            "calc"
        );
        assert_eq!(
            super::resolve_tool_name("calculator", &crate::builtin_tools::builtin_specs()),
            "calc"
        );
        assert_eq!(
            super::resolve_tool_name("`now`", &crate::builtin_tools::builtin_specs()),
            "now"
        );
    }
}
