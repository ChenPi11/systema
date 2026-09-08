//! Minimal block-style YAML emitter with ANSI highlighting, tuned for the
//! unit-state snapshots produced by the System Allocator.
//!
//! The output is deterministic: object keys are always emitted in sorted
//! order.  Highlighting is applied through the `colored` crate, which
//! honours the global override (`colored::control::set_override(..)`) set
//! by the CLI — so the exact same emitter renders plain text when piped and
//! colored text when paging on a terminal.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use colored::Colorize;
use serde_json::Value;

/// Render one unit as a YAML document:
///
/// ```yaml
/// ---
/// foo.service:
///   active_state: active
///   ...
/// ```
pub fn render_doc(w: &mut String, name: &str, value: &Value) -> std::fmt::Result {
    writeln!(w, "{}", "---".dimmed())?;
    writeln!(w, "{}:", name.trim().bold().yellow())?;
    match value {
        Value::Object(map) => emit_map(w, map, 2),
        _ => {
            let mut line = String::new();
            emit_scalar_inline(&mut line, value);
            writeln!(w, "  {line}")
        }
    }
}

fn emit_map(
    w: &mut String,
    map: &serde_json::Map<String, Value>,
    indent: usize,
) -> std::fmt::Result {
    let keys: BTreeSet<&String> = map.keys().collect();
    for key in keys {
        let value = &map[key];
        match value {
            Value::Object(child) if !child.is_empty() => {
                writeln!(w, "{}{}:", pad(indent), key.bold().cyan())?;
                emit_map(w, child, indent + 2)?;
            }
            Value::Array(child) if !child.is_empty() => {
                writeln!(w, "{}{}:", pad(indent), key.bold().cyan())?;
                emit_array(w, child, indent + 2)?;
            }
            Value::Object(_) => {
                writeln!(w, "{}{}: {}", pad(indent), key.bold().cyan(), "{}".dimmed())?;
            }
            Value::Array(_) => {
                writeln!(w, "{}{}: {}", pad(indent), key.bold().cyan(), "[]".dimmed())?;
            }
            other => {
                let mut v = String::new();
                emit_scalar_inline(&mut v, other);
                writeln!(w, "{}{}: {v}", pad(indent), key.bold().cyan())?;
            }
        }
    }
    Ok(())
}

fn emit_array(w: &mut String, items: &[Value], indent: usize) -> std::fmt::Result {
    for item in items {
        match item {
            Value::Object(child) => {
                if child.is_empty() {
                    writeln!(w, "{}- {}", pad(indent), "{}".dimmed())?;
                } else {
                    writeln!(w, "{}- ", pad(indent))?;
                    emit_map(w, child, indent + 2)?;
                }
            }
            Value::Array(child) if !child.is_empty() => {
                emit_array(w, child, indent + 2)?;
            }
            other => {
                let mut v = String::new();
                emit_scalar_inline(&mut v, other);
                writeln!(w, "{}- {v}", pad(indent))?;
            }
        }
    }
    Ok(())
}

fn emit_scalar_inline(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str(&"null".dimmed().to_string()),
        Value::Bool(true) => out.push_str(&"true".magenta().to_string()),
        Value::Bool(false) => out.push_str(&"false".magenta().to_string()),
        Value::Number(n) => out.push_str(&n.to_string().yellow().to_string()),
        Value::String(s) if plain_safe(s) => out.push_str(&s.green().to_string()),
        Value::String(s) => out.push_str(&quote(s).green().to_string()),
        _ => out.push_str(&value.to_string()),
    }
}

/// Double-quote a string using JSON escaping, which is a valid YAML
/// double-quoted scalar (YAML's escape set is a superset of JSON's).
fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// True if the string can be emitted as a plain (unquoted) YAML scalar.
fn plain_safe(s: &str) -> bool {
    if s.is_empty() || s.trim() != s {
        return false;
    }
    // Multi-line values are always quoted (escapes), never block literals.
    if s.contains('\n') || s.contains('\t') {
        return false;
    }
    // YAML indicators at the start of a plain scalar must be quoted.
    let first = s.chars().next().unwrap();
    if "-?:,[]{}#&*!|>'\"%@`".contains(first) {
        return false;
    }
    // Things that would be re-interpreted as other types.
    if ["null", "true", "false", "yes", "no", "on", "off", "~"]
        .contains(&s.to_ascii_lowercase().as_str())
    {
        return false;
    }
    if s.chars().next().unwrap().is_ascii_digit()
        || ((s.starts_with('-') || s.starts_with('+'))
            && s.chars().nth(1).is_some_and(|c| c.is_ascii_digit()))
    {
        return false;
    }
    // "key: value" or end-of-line "# comment" markers must be quoted.
    if s.contains(": ") || s.contains(" #") || s.ends_with(':') {
        return false;
    }
    // Control characters are never plain.
    !s.chars().any(|c| c.is_control())
}

fn pad(indent: usize) -> String {
    " ".repeat(indent)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_nested_yaml_block() {
        let doc = json!({
            "active_state": "active",
            "unit": { "after": ["a.service", "b.service"], "description": "Foo" },
            "jobs": [],
        });
        let mut out = String::new();
        render_doc(&mut out, "foo.service", &doc).unwrap();
        let expected = "---
foo.service:
  active_state: active
  jobs: []
  unit:
    after:
      - a.service
      - b.service
    description: Foo
";
        assert_eq!(out, expected);
    }

    #[test]
    fn quotes_ambiguous_scalars() {
        let doc = json!({ "v": "123", "s": "leave me alone", "n": "null", "c": "a#b" });
        let mut out = String::new();
        render_doc(&mut out, "x.service", &doc).unwrap();
        assert!(out.contains("v: \"123\""));
        assert!(out.contains("s: leave me alone"));
        assert!(out.contains("n: \"null\""));
        // "a#b" is a valid plain scalar (`#` only starts a comment after
        // whitespace), so it must NOT be quoted.
        assert!(out.contains("c: a#b"));
    }

    #[test]
    fn plain_scalar_survives_round_trip() {
        // Every scalar must be emitted in a form that never gets
        // re-interpreted by a YAML parser (i.e. as a string, not a number,
        // bool, comment or mapping).
        for s in [
            "a:b",
            "  x",
            "- dash",
            "1.5s",
            "yes",
            "true",
            "#comment",
            "with\\slash",
            "a b c",
        ] {
            let doc = json!({ "k": s });
            let mut out = String::new();
            render_doc(&mut out, "u.service", &doc).unwrap();
            let line = out.lines().skip(2).next().unwrap();
            let val = line.split_once(": ").map(|(_, v)| v).unwrap_or("");
            if val.starts_with('"') {
                let parsed: Value = serde_json::from_str(val).unwrap();
                assert_eq!(parsed.as_str().unwrap(), s, "{s:?} => {line}");
            } else if matches!(val, "yes" | "true" | "false" | "null" | "~") {
                panic!("{s:?} emitted ambiguously as plain scalar: {line}");
            }
        }
    }
}
