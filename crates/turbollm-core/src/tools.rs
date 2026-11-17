//! Tool calling for local LLMs.
//!
//! Three moving parts live here:
//!
//! 1. **Prompt rendering** — small instruct models only call tools reliably when
//!    the call format matches the one they were fine-tuned on. We support the
//!    formats of the families that actually run on a phone (Qwen2.5/Qwen3,
//!    Hermes/Mistral, Llama-3.1/3.2) and auto-detect the right one from the GGUF
//!    metadata.
//! 2. **Parsing** — the model output is scanned for `<tool_call>`, ```json```,
//!    `<|python_tag|>` or a bare JSON object, in that order.
//! 3. **Grammar (GBNF)** — optional hard guarantee: when `force_json` is set we
//!    hand llama.cpp a grammar that only accepts a valid tool-call object, so a
//!    0.5B model *cannot* emit broken JSON. This is the trick most mobile chat
//!    apps skip; without it small models fail to produce parseable calls
//!    roughly a third of the time.

use serde::{Deserialize, Serialize};

/// One tool the model may call. `parameters` is a JSON-Schema object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema (`{"type":"object","properties":{...},"required":[...]}`).
    #[serde(default = "default_schema")]
    pub parameters: serde_json::Value,
    /// `local` (executed in the app) or `mcp` (executed by an MCP server).
    #[serde(default = "default_kind")]
    pub kind: String,
    /// For `mcp` tools: which server provides it (informational, used by the app).
    #[serde(default)]
    pub server: String,
}

fn default_schema() -> serde_json::Value {
    serde_json::json!({"type": "object", "properties": {}})
}
fn default_kind() -> String {
    "local".to_string()
}

impl ToolSpec {
    /// OpenAI-compatible function object, understood by every tool-tuned model.
    pub fn as_openai_function(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolStyle {
    /// Detect from the model metadata / chat template.
    #[default]
    Auto,
    /// `<tool_call>{"name":…,"arguments":{…}}</tool_call>` — Qwen2.5/3, Hermes
    Qwen,
    /// `<tool_call>…</tool_call>` — Mistral / Hermes (same as Qwen in practice)
    Hermes,
    /// `<|python_tag|>{"name":…,"parameters":{…}}` — Llama 3.1/3.2
    Llama3,
    /// Plain JSON object, no wrapper tags.
    Json,
}

/// A parsed tool call coming out of the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub name: String,
    /// Arguments as a JSON object (already parsed when possible, else a string).
    pub arguments: serde_json::Value,
}

/// Decide which tool syntax to use.
pub fn detect_style(
    description: &str,
    chat_template: Option<&str>,
    forced: ToolStyle,
) -> ToolStyle {
    if forced != ToolStyle::Auto {
        return forced;
    }
    let hay = format!(
        "{} {}",
        description.to_ascii_lowercase(),
        chat_template.unwrap_or("").to_ascii_lowercase()
    );
    // Llama 3.x uses <|python_tag|>; everything else in the "runs on a phone"
    // tier (Qwen2.5/3, Hermes, Mistral, SmolLM2) uses <tool_call> JSON.
    if hay.contains("llama-3") || hay.contains("llama3") || hay.contains("python_tag") {
        ToolStyle::Llama3
    } else {
        ToolStyle::Qwen
    }
}

/// The block appended to the system prompt that teaches the model its tools.
pub fn render_tools_prompt(tools: &[ToolSpec], style: ToolStyle) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let defs: Vec<String> = tools
        .iter()
        .map(|t| {
            // One JSON object per line: cheap to read for small models, and it is
            // what Qwen/Hermes were trained on.
            serde_json::to_string(&t.as_openai_function()).unwrap_or_default()
        })
        .collect();
    let defs = defs.join("\n");

    match style {
        ToolStyle::Llama3 => format!(
            "\n\nYou have access to the following functions:\n\n{defs}\n\n\
             To call a function, output a JSON object with the function name and \
             parameters, wrapped in tags: <|python_tag|>{{\"name\": <name>, \
             \"parameters\": <args>}}. Never call a function that is not listed. \
             If no function is needed, answer normally."
        ),
        ToolStyle::Json => format!(
            "\n\nAvailable tools (JSON functions):\n{defs}\n\n\
             Reply ONLY with a JSON object {{\"name\": <tool>, \"arguments\": <object>}} \
             when a tool is needed."
        ),
        _ => format!(
            "\n\n# Tools\n\nYou may call one or more functions to assist with the user query.\n\
             You are provided with function signatures within <tools></tools> XML tags:\n\
             <tools>\n{defs}\n</tools>\n\
             For each function call, return a json object with function name and arguments \
             within <tool_call></tool_call> XML tags:\n\
             <tool_call>\n{{\"name\": <function-name>, \"arguments\": <args-json-object>}}\n</tool_call>\n\
             Do not call a function that is not listed. If no function is needed, answer normally."
        ),
    }
}

/// GBNF grammar that only accepts a well-formed tool call.
///
/// Used when `force_json` is on: it makes malformed JSON impossible for small
/// models (a 0.5B model otherwise produces `{"name": "x", arguments: ...}` and
/// friends). We cannot restrict `name` to an enum without hurting readability,
/// so the model picks any short string — the app validates it against the
/// registry afterwards.
pub fn tool_grammar(style: ToolStyle) -> String {
    let object = r#"object ::= "{" space "\"name\"" space ":" space string space "," space "\"arguments\"" space ":" space args space "}"
args ::= object | "{}"
value ::= object | array | string | number | ("true" | "false" | "null")
array ::= "[" space (value (space "," space value)*)? space "]"
object ::= "{" space (string space ":" space value (space "," space string space ":" space value)*)? space "}"
string ::= "\"" char* "\""
char ::= [^"\\] | "\\" ["\\/bfnrt]
number ::= "-"? ([0-9] | [1-9] [0-9]*) ("." [0-9]+)? ([eE] [-+]? [0-9]+)?
space ::= [ \t\n]*"#;
    // The grammar above has two `object` rules (a mistake in many blog posts);
    // the real one is built explicitly below.
    let _ = object;
    let body = match style {
        ToolStyle::Llama3 => {
            r#"root ::= "<|python_tag|>" json
json ::= "{" sp "\"name\"" sp ":" sp str sp "," sp "\"parameters\"" sp ":" sp obj sp "}"
"#
        }
        _ => {
            r#"root ::= "<tool_call>\n" json "\n</tool_call>"
json ::= "{" sp "\"name\"" sp ":" sp str sp "," sp "\"arguments\"" sp ":" sp obj sp "}"
"#
        }
    };
    let mut g = String::from(body);
    g.push_str(
        r#"obj ::= "{" sp (pair (sp "," sp pair)*)? sp "}"
pair ::= str sp ":" sp val
val ::= obj | arr | str | num | "true" | "false" | "null"
arr ::= "[" sp (val (sp "," sp val)*)? sp "]"
str ::= "\"" chr* "\""
chr ::= [^"\\] | "\\" ["\\/bfnrtn]
num ::= "-"? [0-9]+ ("." [0-9]+)? ([eE] [-+]? [0-9]+)?
sp ::= [ \t\n]*
"#,
    );
    g
}

/// Extract a tool call from the model output.
pub fn parse_tool_call(text: &str, style: ToolStyle) -> Option<ToolCall> {
    // 1. <tool_call> ... </tool_call>  (Qwen / Hermes / Mistral)
    if let Some(start) = text.find("<tool_call>") {
        let rest = &text[start + "<tool_call>".len()..];
        let body = rest.split("</tool_call>").next().unwrap_or(rest);
        if let Some(call) = parse_call_json(body) {
            return Some(call);
        }
    }
    // 2. <|python_tag|> {...} (Llama 3.x)
    if let Some(idx) = text.find("<|python_tag|>") {
        let rest = &text[idx + "<|python_tag|>".len()..];
        if let Some(call) = parse_call_json(rest) {
            return Some(call);
        }
    }
    // 3. fenced ```json {...} ```
    if let Some(idx) = text.find("```") {
        let rest = &text[idx + 3..];
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        let body = rest.split("```").next().unwrap_or(rest);
        if let Some(call) = parse_call_json(body) {
            return Some(call);
        }
    }
    // 4. bare JSON object (style Json / grammar-locked output)
    if matches!(style, ToolStyle::Json) || text.trim_start().starts_with('{') {
        if let Some(call) = parse_call_json(text) {
            return Some(call);
        }
    }
    None
}

fn parse_call_json(raw: &str) -> Option<ToolCall> {
    let candidate = extract_json_object(raw)?;
    let v: serde_json::Value = serde_json::from_str(&candidate).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    // Models interchangeably use "arguments" / "parameters" / "args".
    let arguments = v
        .get("arguments")
        .or_else(|| v.get("parameters"))
        .or_else(|| v.get("args"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let arguments = match arguments {
        serde_json::Value::String(s) => {
            serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s))
        }
        other => other,
    };
    Some(ToolCall { name, arguments })
}

/// Take the first balanced `{...}` block out of a string (models love prose).
fn extract_json_object(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_qwen_style() {
        let out = "Sure!\n<tool_call>\n{\"name\": \"web_fetch\", \"arguments\": {\"url\": \"https://x\"}}\n</tool_call>";
        let call = parse_tool_call(out, ToolStyle::Qwen).unwrap();
        assert_eq!(call.name, "web_fetch");
        assert_eq!(call.arguments["url"], "https://x");
    }

    #[test]
    fn parses_llama_style() {
        let out = "<|python_tag|>{\"name\":\"clock\",\"parameters\":{}}";
        let call = parse_tool_call(out, ToolStyle::Llama3).unwrap();
        assert_eq!(call.name, "clock");
    }

    #[test]
    fn parses_stringified_arguments() {
        let out = "<tool_call>{\"name\":\"calc\",\"arguments\":\"{\\\"expr\\\":\\\"2+2\\\"}\"}</tool_call>";
        let call = parse_tool_call(out, ToolStyle::Qwen).unwrap();
        assert_eq!(call.arguments["expr"], "2+2");
    }

    #[test]
    fn ignores_plain_answers() {
        assert!(parse_tool_call("Привет! Чем помочь?", ToolStyle::Qwen).is_none());
    }

    #[test]
    fn prompt_contains_schema() {
        let tools = vec![ToolSpec {
            name: "clock".into(),
            description: "current time".into(),
            parameters: default_schema(),
            kind: "local".into(),
            server: String::new(),
        }];
        let p = render_tools_prompt(&tools, ToolStyle::Qwen);
        assert!(p.contains("<tool_call>"));
        assert!(p.contains("\"clock\""));
    }
}
