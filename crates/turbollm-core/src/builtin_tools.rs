//! Built-in tools: the local "agent" capabilities that work offline.
//!
//! Everything here runs inside the app process, needs no server and no extra
//! permissions, which is the point: a phone assistant should be useful without
//! a network. Tools that need the network (HTTP) are executed by the Kotlin side
//! through a callback, so the Rust core stays free of TLS stacks.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::tools::ToolSpec;

/// Perform a single arithmetic/expression calculation — implemented as a tiny
/// safe evaluator (no `eval`, no dependency), supports + - * / % ^, parentheses,
/// unary minus and the functions sqrt, sin, cos, tan, log, ln, abs, round, floor,
/// ceil, min, max. Angles in radians.
pub fn calculate(expr: &str) -> Result<f64, String> {
    let mut p = Parser {
        s: expr.as_bytes(),
        i: 0,
    };
    let v = p.expr()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(format!("лишние символы в выражении: «{}»", &expr[p.i..]));
    }
    if !v.is_finite() {
        return Err("результат не является конечным числом".into());
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && (self.s[self.i] as char).is_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expr(&mut self) -> Result<f64, String> {
        let mut v = self.term()?;
        loop {
            if self.eat(b'+') {
                v += self.term()?;
            } else if self.eat(b'-') {
                v -= self.term()?;
            } else {
                return Ok(v);
            }
        }
    }

    fn term(&mut self) -> Result<f64, String> {
        let mut v = self.power()?;
        loop {
            if self.eat(b'*') {
                v *= self.power()?;
            } else if self.eat(b'/') {
                let d = self.power()?;
                if d == 0.0 {
                    return Err("деление на ноль".into());
                }
                v /= d;
            } else if self.eat(b'%') {
                let d = self.power()?;
                if d == 0.0 {
                    return Err("деление на ноль".into());
                }
                v %= d;
            } else {
                return Ok(v);
            }
        }
    }

    fn power(&mut self) -> Result<f64, String> {
        let base = self.unary()?;
        if self.eat(b'^') {
            let exp = self.power()?; // right-associative
            return Ok(base.powf(exp));
        }
        Ok(base)
    }

    fn unary(&mut self) -> Result<f64, String> {
        if self.eat(b'-') {
            return Ok(-self.unary()?);
        }
        if self.eat(b'+') {
            return self.unary();
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some(b'(') => {
                self.i += 1;
                let v = self.expr()?;
                if !self.eat(b')') {
                    return Err("не закрыта скобка".into());
                }
                Ok(v)
            }
            Some(c) if c.is_ascii_digit() || c == b'.' => {
                let start = self.i;
                while self.i < self.s.len()
                    && (self.s[self.i].is_ascii_digit()
                        || self.s[self.i] == b'.'
                        || self.s[self.i] == b'e'
                        || self.s[self.i] == b'E')
                {
                    // allow exponent sign
                    if (self.s[self.i] == b'e' || self.s[self.i] == b'E')
                        && self.s.get(self.i + 1).map(|c| *c == b'-' || *c == b'+') == Some(true)
                    {
                        self.i += 2;
                        continue;
                    }
                    self.i += 1;
                }
                let txt = std::str::from_utf8(&self.s[start..self.i]).map_err(|e| e.to_string())?;
                txt.parse::<f64>().map_err(|_| format!("не число: «{txt}»"))
            }
            Some(c) if c.is_ascii_alphabetic() => {
                let start = self.i;
                while self.i < self.s.len() && self.s[self.i].is_ascii_alphabetic() {
                    self.i += 1;
                }
                let name =
                    std::str::from_utf8(&self.s[start..self.i]).map_err(|e| e.to_string())?;
                let name = name.to_ascii_lowercase();
                if name == "pi" {
                    return Ok(std::f64::consts::PI);
                }
                if name == "e" {
                    return Ok(std::f64::consts::E);
                }
                if !self.eat(b'(') {
                    return Err(format!("функция {name} без аргументов"));
                }
                let mut args = vec![self.expr()?];
                while self.eat(b',') {
                    args.push(self.expr()?);
                }
                if !self.eat(b')') {
                    return Err("не закрыта скобка в функции".into());
                }
                let a = |n: usize| args.get(n).copied().unwrap_or(0.0);
                Ok(match name.as_str() {
                    "sqrt" => a(0).sqrt(),
                    "sin" => a(0).sin(),
                    "cos" => a(0).cos(),
                    "tan" => a(0).tan(),
                    "asin" => a(0).asin(),
                    "acos" => a(0).acos(),
                    "atan" => a(0).atan(),
                    "log" | "log10" => a(0).log10(),
                    "ln" => a(0).ln(),
                    "exp" => a(0).exp(),
                    "abs" => a(0).abs(),
                    "round" => a(0).round(),
                    "floor" => a(0).floor(),
                    "ceil" => a(0).ceil(),
                    "min" => args.iter().copied().fold(f64::INFINITY, f64::min),
                    "max" => args.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                    "pow" => a(0).powf(a(1)),
                    other => return Err(format!("неизвестная функция: {other}")),
                })
            }
            _ => Err("пустое или некорректное выражение".into()),
        }
    }
}

/// Tool used when the model has no tools of its own but the user wants the
/// built-ins: time, calculation, safe local knowledge.
pub fn builtin_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "now".into(),
            description: "Текущая дата и время устройства (локальная зона)".into(),
            parameters: json!({"type": "object", "properties": {}}),
            kind: "local".into(),
            server: "builtin".into(),
        },
        ToolSpec {
            name: "calc".into(),
            description: "Вычислить арифметическое выражение. Поддерживает + - * / % ^, \
                          скобки, sqrt, sin, cos, tan, log, ln, abs, round, min, max."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"expr": {"type": "string", "description": "например (2+3)*4.5"}},
                "required": ["expr"]
            }),
            kind: "local".into(),
            server: "builtin".into(),
        },
        ToolSpec {
            name: "device_info".into(),
            description:
                "Информация об устройстве: ABI, число ядер, версия Android, свободная память."
                    .into(),
            parameters: json!({"type": "object", "properties": {}}),
            kind: "android".into(),
            server: "builtin".into(),
        },
    ]
}

/// Execute a built-in tool. `android` tools are delegated to Kotlin via the JNI
/// callback, so this returns `None` for them.
pub fn run_builtin(name: &str, args: &Value) -> Option<Result<String, String>> {
    match name {
        "now" => {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            Some(Ok(format!("unix={secs}; utc={}", iso8601_utc(secs as i64))))
        }
        "calc" => Some(match find_expr(args) {
            Some(e) => calculate(&e).map(|v| {
                if v.fract() == 0.0 && v.abs() < 1e15 {
                    format!("{}", v as i64)
                } else {
                    format!("{v}")
                }
            }),
            None => Err("нужен параметр expr".into()),
        }),
        _ => None,
    }
}

/// Models name the single argument of `calc` differently every time
/// (`expr`, `expression`, `arg`, `input`, …). Accept all of them, and fall back
/// to the only string value if the object has exactly one.
fn find_expr(args: &Value) -> Option<String> {
    const ALIASES: [&str; 7] = [
        "expr",
        "expression",
        "arg",
        "args",
        "input",
        "query",
        "text",
    ];
    for key in ALIASES {
        if let Some(Value::String(s)) = args.get(key) {
            return Some(s.clone());
        }
        if let Some(v) = args.get(key) {
            if v.is_number() {
                return Some(v.to_string());
            }
        }
    }
    if let Some(Value::Object(map)) = Some(args) {
        if map.len() == 1 {
            let (_, v) = map.iter().next()?;
            match v {
                Value::String(s) => return Some(s.clone()),
                Value::Number(n) => return Some(n.to_string()),
                _ => {}
            }
        }
    }
    args.as_str().map(|s| s.to_string())
}

/// Minimal ISO-8601 formatter (no chrono dependency in the mobile build).
fn iso8601_utc(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (h, mi, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic() {
        assert_eq!(calculate("2+2").unwrap(), 4.0);
        assert_eq!(calculate("(2+3)*4.5").unwrap(), 22.5);
        assert_eq!(calculate("2^3^2").unwrap(), 512.0); // right associative
        assert_eq!(calculate("-3 + 10 % 4").unwrap(), -1.0);
        assert!((calculate("sqrt(16) + max(1,2,3)").unwrap() - 7.0).abs() < 1e-9);
        assert!((calculate("sin(pi/2)").unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn errors_are_reported() {
        assert!(calculate("1/0").is_err());
        assert!(calculate("2+").is_err());
        assert!(calculate("foo(1)").is_err());
        assert!(calculate("(1+2").is_err());
        assert!(calculate("1+2)").is_err());
    }

    #[test]
    fn calc_accepts_argument_aliases() {
        for key in ["expr", "expression", "arg", "input", "query", "text"] {
            let r = run_builtin("calc", &json!({ key: "10/4" }))
                .unwrap()
                .unwrap();
            assert_eq!(r, "2.5", "alias {key}");
        }
        // единственный аргумент с «неправильным» именем
        let r = run_builtin("calc", &json!({"formula": "7*6"}))
            .unwrap()
            .unwrap();
        assert_eq!(r, "42");
    }

    #[test]
    fn runs_builtins() {
        let r = run_builtin("calc", &json!({"expr": "6*7"}))
            .unwrap()
            .unwrap();
        assert_eq!(r, "42");
        let now = run_builtin("now", &json!({})).unwrap().unwrap();
        assert!(now.contains("utc="));
        assert!(run_builtin("unknown", &json!({})).is_none());
    }

    #[test]
    fn iso_format() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn specs_have_schemas() {
        let specs = builtin_specs();
        assert!(specs.iter().any(|s| s.name == "calc"));
        assert!(specs.iter().find(|s| s.name == "calc").unwrap().parameters["required"].is_array());
    }
}
