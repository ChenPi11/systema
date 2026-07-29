//! Process management for System S.
//!
//! Provides `start_service` and `stop_service` functions that launch and
//! terminate processes, respectively. Uses `tokio::process` (which wraps
//! fork/exec on Unix) so we remain single-threaded-async.
//!
//! # ExecStart parsing (systemd-compatible)
//!
//! Systemd's `ExecStart=` directive supports several single-character
//! prefixes, `%`-specifiers, `$VAR` / `${VAR}` environment expansion,
//! and a `|` prefix that routes the command through `sh -c`.  Our
//! implementation mirrors systemd's behaviour:
//!
//! | Prefix | systemd flag               | Meaning                          |
//! |--------|----------------------------|----------------------------------|
//! | `-`    | `IGNORE_FAILURE`           | Ignore non-zero exit             |
//! | `@`    | (separate argv[0])         | Use next token as argv[0]        |
//! | `+`    | `FULLY_PRIVILEGED`         | Run as root (no User=/Group=)    |
//! | `:`    | `NO_ENV_EXPAND`            | Disable `$VAR` expansion         |
//! | `!`    | `NO_SETUID`                | No credential changes            |
//! | `!!`   | `NO_SETUID` (seccomp only) | Like `!` but only for seccomp    |
//! | `|`    | `VIA_SHELL`                | Run via `sh -c`                  |

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

use sysa::proto::UnitConfig;

use crate::state::{ServiceInstance, ServiceRegistry, ServiceState};

/// Launch the service described by `config`.
/// Returns the PID and the Child handle of the spawned main process.
/// The caller must keep the Child handle to later collect the exit status.
///
/// If `invocation_id` is `Some`, the `INVOCATION_ID` environment variable is
/// set in the spawned process's environment (systemd-compatible behaviour).
pub async fn start_service(
    registry: ServiceRegistry,
    config: &UnitConfig,
    invocation_id: Option<String>,
) -> Result<(u32, Child)> {
    let unit_name = config.unit_name.clone();
    let svc = config
        .service
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!(sysa::l10n::fmt(sysa::l10n::t_("No [Service] config for {unit_name}."), &[("unit_name", &unit_name)])))?;

    if svc.exec_start.is_empty() {
        bail!(sysa::l10n::fmt(sysa::l10n::t_("ExecStart is empty for {unit_name}."), &[("unit_name", &unit_name)]));
    }

    // Parse the ExecStart command line (prefixes, word splitting, % specifiers).
    let parsed = parse_exec_start(&svc.exec_start, &unit_name)?;
    info!("Starting {}: {} {:?}", unit_name, parsed.program, parsed.args);

    // Build environment lookup table (process env + unit Environment=).
    let env_table = build_env_table(&svc.environment);

    // Expand $VAR / ${VAR} in every argument, handling standalone splitting.
    let final_args = if parsed.flags.no_env_expand {
        parsed.args.clone()
    } else {
        expand_argv(&parsed.args, &env_table)
    };

    // Update state to Starting.
    {
        let mut reg = registry.lock();
        let inst = reg
            .entry(unit_name.clone())
            .or_insert_with(|| ServiceInstance::new(unit_name.clone()));
        inst.state = ServiceState::Starting;
    }

    // Build the Command.
    let mut cmd = if parsed.flags.via_shell {
        // | prefix: route through sh -c
        let joined = build_shell_command_line(&parsed.program, &final_args);
        let mut c = Command::new(sysa::paths::instance().systema_shell_path);
        c.arg("-c");
        c.arg(&joined);
        c
    } else {
        let mut c = Command::new(&parsed.program);
        c.args(&final_args);
        c
    };

    if !svc.working_directory.is_empty() {
        cmd.current_dir(&svc.working_directory);
    }

    // Environment variables.
    for env_str in &svc.environment {
        if let Some((key, val)) = env_str.split_once('=') {
            cmd.env(key, val);
        }
    }

    // Set INVOCATION_ID if provided (systemd compatibility).
    if let Some(ref inv_id) = invocation_id {
        cmd.env("INVOCATION_ID", inv_id);
    }

    // Spawn the child process. We deliberately do NOT wait here — the child
    // is monitored asynchronously via `monitor_child`.
    let child = cmd
        .spawn()
        .with_context(|| sysa::l10n::fmt(sysa::l10n::t_("Failed to spawn {program}."), &[("program", &parsed.program)]))?;

    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!(sysa::l10n::fmt(sysa::l10n::t_("Failed to get PID for {unit_name}."), &[("unit_name", &unit_name)])))?;

    info!("Service {} started, PID={}", unit_name, pid);

    // Update state to Running.
    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(&unit_name) {
            inst.state = ServiceState::Running;
            inst.main_pid = Some(pid);
        }
    }

    Ok((pid, child))
}

/// Stop a running service by sending SIGTERM (then SIGKILL after timeout).
pub async fn stop_service(
    registry: ServiceRegistry,
    unit_name: &str,
    timeout_secs: u32,
) -> Result<()> {
    let pid = {
        let mut reg = registry.lock();
        let inst = reg.get_mut(unit_name);
        match inst {
            None => {
                debug!("stop_service: {} not in registry", unit_name);
                return Ok(());
            }
            Some(inst) => {
                if inst.state == ServiceState::Dead || inst.state == ServiceState::Failed {
                    debug!("stop_service: {} already stopped", unit_name);
                    return Ok(());
                }
                inst.state = ServiceState::Stopping;
                inst.main_pid
            }
        }
    };

    if let Some(pid) = pid {
        #[cfg(unix)]
        {
            use nix::sys::signal::{self, Signal};
            use nix::unistd::Pid;
            let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
            info!("Sent SIGTERM to PID {} ({})", pid, unit_name);

            let deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(timeout_secs.max(1) as u64);
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                if !is_alive(pid) {
                    info!("Service {} (PID {}) exited after SIGTERM", unit_name, pid);
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    warn!(
                        "Service {} (PID {}) did not exit in {}s; sending SIGKILL",
                        unit_name, pid, timeout_secs
                    );
                    let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
                    break;
                }
            }
        }
        #[cfg(not(unix))]
        {
            warn!("Signal delivery not supported on this platform");
        }
    }

    {
        let mut reg = registry.lock();
        if let Some(inst) = reg.get_mut(unit_name) {
            inst.state = ServiceState::Dead;
            inst.main_pid = None;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Platform-specific helpers
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    use nix::sys::signal;
    use nix::unistd::Pid;
    signal::kill(Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(not(unix))]
pub fn is_alive(_pid: u32) -> bool {
    false
}

// ---------------------------------------------------------------------------
// ExecStart parsing (systemd-compatible)
// ---------------------------------------------------------------------------

/// Flags collected from systemd-style prefix characters in `ExecStart=`.
#[derive(Debug, Clone, Default)]
struct ExecFlags {
    ignore_failure: bool,
    privileged: bool,
    custom_argv0: bool,
    no_env_expand: bool,
    no_new_privileges: bool,
    via_shell: bool,
}

/// Result of parsing an `ExecStart=` line.
#[derive(Debug, Clone)]
struct ParsedExec {
    program: String,
    args: Vec<String>,
    flags: ExecFlags,
}

/// Parse a raw `ExecStart=` string into program + args, after stripping
/// prefix characters, splitting tokens (systemd-compatible word splitting),
/// and expanding `%`-specifiers.
fn parse_exec_start(raw: &str, unit_name: &str) -> Result<ParsedExec> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!(sysa::l10n::t_("Empty ExecStart command."));
    }

    let (flags, rest) = strip_prefixes(raw);

    // Split the remainder into words (respects quotes, C-escapes).
    let words = split_words(rest);
    if words.is_empty() {
        bail!(sysa::l10n::t_("Empty ExecStart command after prefix stripping."));
    }

    // Expand % specifiers in each word.
    let words: Vec<String> = words
        .into_iter()
        .map(|w| expand_specifiers(&w, unit_name))
        .collect();

    if flags.custom_argv0 {
        // @ prefix: first word is argv[0], second word is the program.
        if words.len() < 2 {
            bail!(sysa::l10n::t_("@ prefix requires at least two tokens (argv0 program)."));
        }
        // We don't have a way to set argv[0] natively in tokio::process::Command,
        // so we just use the program as-is and note the custom argv0 in the log.
        debug!(
            "@ prefix: argv[0] would be '{}', using program '{}'",
            words[0], words[1]
        );
        Ok(ParsedExec {
            program: words[1].clone(),
            args: words[2..].to_vec(),
            flags,
        })
    } else {
        Ok(ParsedExec {
            program: words[0].clone(),
            args: words[1..].to_vec(),
            flags,
        })
    }
}

/// Strip systemd prefix characters from the beginning of a command string.
/// Returns (flags, remainder).
fn strip_prefixes(s: &str) -> (ExecFlags, &str) {
    let mut flags = ExecFlags::default();
    let mut cursor = s;

    loop {
        match cursor.as_bytes().first() {
            Some(b'-') => {
                flags.ignore_failure = true;
                cursor = &cursor[1..];
            }
            Some(b'+') => {
                flags.privileged = true;
                cursor = &cursor[1..];
            }
            Some(b'@') => {
                // The @ prefix means first token is used as argv[0].
                // We track it, but actual argv[0] manipulation is not
                // supported by tokio::process::Command on all platforms.
                flags.custom_argv0 = true;
                cursor = &cursor[1..];
            }
            Some(b':') => {
                flags.no_env_expand = true;
                cursor = &cursor[1..];
            }
            Some(b'!') => {
                flags.no_new_privileges = true;
                cursor = &cursor[1..];
                if cursor.as_bytes().first() == Some(&b'!') {
                    cursor = &cursor[1..];
                }
            }
            Some(b'|') => {
                flags.via_shell = true;
                cursor = &cursor[1..];
            }
            _ => break,
        }
    }

    (flags, cursor)
}

// ---------------------------------------------------------------------------
// Word splitting (systemd extract_first_word compatible)
// ---------------------------------------------------------------------------

/// Split a command-line string into words, respecting single/double quotes
/// and C-style escape sequences (\n, \t, \r, \\, \xNN, \uNNNN, \UNNNNNNNN).
///
/// This mirrors systemd's `extract_first_word` with the flags
/// `EXTRACT_UNQUOTE | EXTRACT_CUNESCAPE`.
fn split_words(s: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;

    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];

        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
            i += 1;
            continue;
        }

        if in_double {
            match ch {
                '\\' => {
                    if i + 1 < chars.len() {
                        i += 1;
                        let next = chars[i];
                        if matches!(next, '"' | '\\' | '$' | '`') {
                            current.push(next);
                        } else {
                            current.push('\\');
                            current.push(next);
                        }
                    } else {
                        current.push('\\');
                    }
                }
                '"' => {
                    in_double = false;
                }
                _ => {
                    current.push(ch);
                }
            }
            i += 1;
            continue;
        }

        match ch {
            ' ' | '\t' => {
                if !current.is_empty() {
                    words.push(current.drain(..).collect());
                }
                i += 1;
            }
            '\'' => {
                in_single = true;
                i += 1;
            }
            '"' => {
                in_double = true;
                i += 1;
            }
            '\\' => {
                if i + 1 < chars.len() {
                    i += 1;
                    let next = chars[i];
                    match next {
                        'a' => current.push('\u{0007}'),
                        'b' => current.push('\u{0008}'),
                        'f' => current.push('\u{000C}'),
                        'n' => current.push('\n'),
                        'r' => current.push('\r'),
                        't' => current.push('\t'),
                        'v' => current.push('\u{000B}'),
                        '\\' => current.push('\\'),
                        '\'' => current.push('\''),
                        '"' => current.push('"'),
                        'x' | 'X' => {
                            let (consumed, byte) = parse_hex_escape(&chars, i + 1);
                            if let Some(b) = byte {
                                current.push(b as char);
                                i += consumed;
                            } else {
                                current.push(next);
                            }
                        }
                        'u' => {
                            let (consumed, c) = parse_unicode_escape(&chars, i + 1, 4);
                            if let Some(c) = c {
                                current.push(c);
                                i += consumed;
                            } else {
                                current.push(next);
                            }
                        }
                        'U' => {
                            let (consumed, c) = parse_unicode_escape(&chars, i + 1, 8);
                            if let Some(c) = c {
                                current.push(c);
                                i += consumed;
                            } else {
                                current.push(next);
                            }
                        }
                        other => {
                            current.push(other);
                        }
                    }
                } else {
                    current.push('\\');
                }
                i += 1;
            }
            other => {
                current.push(other);
                i += 1;
            }
        }
    }

    if !current.is_empty() {
        words.push(current);
    }

    words
}

/// Parse \xNN hex escape starting at index `start` in `chars`.
fn parse_hex_escape(chars: &[char], start: usize) -> (usize, Option<u8>) {
    if start + 1 >= chars.len() {
        return (0, None);
    }
    let hex: String = chars[start..].iter().take(2).collect();
    if hex.len() < 2 {
        return (0, None);
    }
    u8::from_str_radix(&hex, 16).ok().map(|b| (2, Some(b))).unwrap_or((0, None))
}

/// Parse \uNNNN or \UNNNNNNNN unicode escape.
fn parse_unicode_escape(chars: &[char], start: usize, digits: usize) -> (usize, Option<char>) {
    if start + digits > chars.len() {
        return (0, None);
    }
    let hex: String = chars[start..start + digits].iter().collect();
    match u32::from_str_radix(&hex, 16).ok() {
        Some(code) => char::from_u32(code).map(|c| (digits, Some(c))).unwrap_or((0, None)),
        None => (0, None),
    }
}

// ---------------------------------------------------------------------------
// % specifier expansion
// ---------------------------------------------------------------------------

/// Expand systemd `%`-specifiers in `s` using the given unit `name`.
fn expand_specifiers(s: &str, name: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }

    let unit_name = name;
    let unit_no_ext = name.rsplit_once('.').map(|(p, _)| p).unwrap_or(name);
    let (prefix, instance) = if let Some(at_pos) = unit_no_ext.find('@') {
        let p = &unit_no_ext[..at_pos];
        let i = &unit_no_ext[at_pos + 1..];
        (p, i)
    } else {
        (unit_no_ext, "")
    };

    let hostname = || {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let machine_id = || {
        std::fs::read_to_string(sysa::paths::instance().systemd_machine_id_file)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let kernel_release = || {
        std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };

    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            None => out.push('%'),
            Some('n') => out.push_str(unit_name),
            Some('N') => out.push_str(unit_no_ext),
            Some('p') => out.push_str(prefix),
            Some('i') => out.push_str(instance),
            Some('u') => {
                let user = std::env::var("USER")
                    .or_else(|_| std::env::var("LOGNAME"))
                    .unwrap_or_default();
                out.push_str(&user);
            }
            Some('U') => {
                let uid = std::process::Command::new("id")
                    .arg("-u")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                out.push_str(&uid);
            }
            Some('g') => {
                let group = std::env::var("GROUP")
                    .or_else(|_| {
                        std::process::Command::new("id")
                            .arg("-gn")
                            .output()
                            .ok()
                            .and_then(|o| String::from_utf8(o.stdout).ok())
                            .map(|s| s.trim().to_string())
                            .ok_or(std::env::VarError::NotPresent)
                    })
                    .unwrap_or_default();
                out.push_str(&group);
            }
            Some('G') => {
                let gid = std::process::Command::new("id")
                    .arg("-g")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                out.push_str(&gid);
            }
            Some('H') => out.push_str(&hostname()),
            Some('m') => out.push_str(&machine_id()),
            Some('v') => out.push_str(&kernel_release()),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// $VAR / ${VAR} environment variable expansion
// ---------------------------------------------------------------------------

/// Build a lookup table from the unit's `Environment=` settings.
fn build_env_table(unit_env: &[String]) -> HashMap<String, String> {
    let mut table: HashMap<String, String> = HashMap::new();

    for (key, val) in std::env::vars() {
        table.insert(key, val);
    }

    for entry in unit_env {
        if let Some((key, val)) = entry.split_once('=') {
            table.insert(key.to_string(), val.to_string());
        }
    }

    table
}

/// Expand `$VAR` / `${VAR}` in an argv, handling:
///
/// - Standalone `$VAR` (exact word = `$NAME`): value is split by whitespace
///   into multiple argv entries.
/// - `${VAR}` / `$VAR` inline: expanded in-place.
/// - `${VAR:-default}` / `${VAR:+alternate}`: default/alternate value.
fn expand_argv(args: &[String], env: &HashMap<String, String>) -> Vec<String> {
    let mut result = Vec::with_capacity(args.len());

    for arg in args {
        if is_standalone_var_ref(arg) {
            let name = &arg[1..];
            match env.get(name) {
                Some(value) if !value.is_empty() => {
                    for val in value.split_whitespace() {
                        result.push(val.to_string());
                    }
                }
                _ => {
                    // Unset or empty → skip (systemd behaviour).
                }
            }
        } else {
            result.push(expand_env_in_word(arg, env));
        }
    }

    result
}

/// Check if a word is exactly `$VARNAME` (no braces, no surrounding text).
fn is_standalone_var_ref(word: &str) -> bool {
    let bytes = word.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'$' {
        return false;
    }
    if bytes[1] == b'{' {
        return false;
    }
    bytes[1..].iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Expand `${VAR}`, `${VAR:-default}`, `${VAR:+alternate}`, `$VAR`, `$$` within
/// a single word.  This mirrors systemd's `replace_env_full`.
fn expand_env_in_word(word: &str, env: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }

        if chars.peek() == Some(&'$') {
            chars.next();
            out.push('$');
            continue;
        }

        if chars.peek() == Some(&'{') {
            chars.next();
            expand_braced_expr(&mut out, &mut chars, env);
            continue;
        }

        // $VAR — simple variable name.
        let mut name = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_alphanumeric() || c == '_' {
                name.push(c);
                chars.next();
            } else {
                break;
            }
        }

        if name.is_empty() {
            out.push('$');
            continue;
        }

        out.push_str(&env.get(&name).cloned().unwrap_or_default());
    }

    out
}

/// Expand `${...}` expression: `${VAR}`, `${VAR:-default}`, `${VAR:+alternate}`.
fn expand_braced_expr(
    out: &mut String,
    chars: &mut std::iter::Peekable<std::str::Chars>,
    env: &HashMap<String, String>,
) {
    let mut name = String::new();
    let mut substitution: Option<(char, String)> = None;

    enum Phase { Name, Default }
    let mut phase = Phase::Name;
    let mut op = ' '; // '-' or '+'

    loop {
        match chars.next() {
            None => {
                out.push_str("${");
                out.push_str(&name);
                if let Some((_, ref val)) = substitution {
                    out.push(':');
                    out.push_str(val);
                }
                break;
            }
            Some('}') => {
                let resolved = match phase {
                    Phase::Name => env.get(&name).cloned().unwrap_or_default(),
                    Phase::Default => {
                        if op == '-' {
                            env.get(&name)
                                .map(|v| {
                                    if v.is_empty() {
                                        substitution.as_ref().map(|(_, d)| d.clone()).unwrap_or_default()
                                    } else {
                                        v.clone()
                                    }
                                })
                                .unwrap_or_else(|| {
                                    substitution.as_ref().map(|(_, d)| d.clone()).unwrap_or_default()
                                })
                        } else {
                            // '+'
                            if env.get(&name).is_some_and(|v| !v.is_empty()) {
                                substitution.as_ref().map(|(_, a)| a.clone()).unwrap_or_default()
                            } else {
                                String::new()
                            }
                        }
                    }
                };
                out.push_str(&resolved);
                break;
            }
            Some(':') if matches!(phase, Phase::Name) => {
                if let Some(&c @ ('-' | '+')) = chars.peek() {
                    op = c;
                    phase = Phase::Default;
                    chars.next();
                } else {
                    name.push(':');
                }
            }
            Some(ch) if matches!(phase, Phase::Name) => {
                name.push(ch);
            }
            Some(ch) if matches!(phase, Phase::Default) => {
                match substitution {
                    Some((opchar, ref mut val)) => {
                        if opchar != ch as char {
                        }
                        val.push(ch);
                    }
                    None => {
                        let mut val = String::new();
                        val.push(ch);
                        substitution = Some((op, val));
                    }
                }
            }
            _ => unreachable!(),
        }
    }
}

/// Build the command line string for `sh -c` invocation (| prefix).
fn build_shell_command_line(program: &str, args: &[String]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(1 + args.len());
    parts.push(quote_for_shell(program));
    for arg in args {
        parts.push(quote_for_shell(arg));
    }
    parts.join(" ")
}

/// Quote a string for shell consumption (single-quote wrapping).
fn quote_for_shell(s: &str) -> String {
    if s.contains('\'') {
        let mut out = String::new();
        out.push('\'');
        for ch in s.chars() {
            if ch == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(ch);
            }
        }
        out.push('\'');
        out
    } else {
        format!("'{}'", s)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Word splitting ---

    #[test]
    fn split_simple_words() {
        assert_eq!(split_words("foo bar baz"), vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn split_single_quoted() {
        assert_eq!(split_words("foo 'bar baz'"), vec!["foo", "bar baz"]);
    }

    #[test]
    fn split_double_quoted() {
        assert_eq!(split_words("foo \"bar baz\""), vec!["foo", "bar baz"]);
    }

    #[test]
    fn split_mixed_quotes() {
        assert_eq!(
            split_words("foo 'bar \"baz\"' \"qux 'quux'\""),
            vec!["foo", "bar \"baz\"", "qux 'quux'"]
        );
    }

    #[test]
    fn split_backslash_escape() {
        assert_eq!(split_words(r"foo\ bar"), vec!["foo bar"]);
    }

    #[test]
    fn split_c_escape_newline() {
        assert_eq!(split_words(r"foo\nbar"), vec!["foo\nbar"]);
    }

    #[test]
    fn split_c_escape_tab() {
        assert_eq!(split_words(r"foo\tbar"), vec!["foo\tbar"]);
    }

    #[test]
    fn split_c_escape_hex() {
        assert_eq!(split_words(r"foo\x20bar"), vec!["foo bar"]);
    }

    #[test]
    fn split_c_escape_unicode() {
        assert_eq!(split_words(r"foo\u0020bar"), vec!["foo bar"]);
    }

    #[test]
    fn double_quote_backslash_escapes() {
        assert_eq!(split_words(r#""foo\"bar""#), vec!["foo\"bar"]);
        assert_eq!(split_words(r#""foo\\bar""#), vec!["foo\\bar"]);
    }

    #[test]
    fn double_quote_backslash_n_is_literal() {
        assert_eq!(split_words(r#""foo\nbar""#), vec![r"foo\nbar"]);
    }

    #[test]
    fn split_tabs_as_separators() {
        assert_eq!(split_words("foo\tbar"), vec!["foo", "bar"]);
    }

    // --- Prefix stripping ---

    #[test]
    fn strip_ignore_failure() {
        let (f, rest) = strip_prefixes("-/usr/bin/foo");
        assert!(f.ignore_failure);
        assert_eq!(rest, "/usr/bin/foo");
    }

    #[test]
    fn strip_privileged() {
        let (f, rest) = strip_prefixes("+/usr/bin/foo");
        assert!(f.privileged);
        assert_eq!(rest, "/usr/bin/foo");
    }

    #[test]
    fn strip_via_shell() {
        let (f, rest) = strip_prefixes("|/usr/bin/foo arg");
        assert!(f.via_shell);
        assert_eq!(rest, "/usr/bin/foo arg");
    }

    #[test]
    fn strip_no_env_expand() {
        let (f, rest) = strip_prefixes(":/usr/bin/foo");
        assert!(f.no_env_expand);
        assert_eq!(rest, "/usr/bin/foo");
    }

    #[test]
    fn strip_combined() {
        let (f, rest) = strip_prefixes("-+/usr/bin/foo");
        assert!(f.ignore_failure);
        assert!(f.privileged);
        assert_eq!(rest, "/usr/bin/foo");
    }

    #[test]
    fn strip_double_bang() {
        let (f, rest) = strip_prefixes("!!/usr/bin/foo");
        assert!(f.no_new_privileges);
        assert_eq!(rest, "/usr/bin/foo");
    }

    #[test]
    fn strip_at_prefix() {
        let (f, rest) = strip_prefixes("@/usr/lib/foo/foo myapp");
        assert!(f.custom_argv0);
        assert_eq!(rest, "/usr/lib/foo/foo myapp");
    }

    #[test]
    fn strip_all_prefixes() {
        let (f, rest) = strip_prefixes("-+@:/usr/bin/foo");
        assert!(f.ignore_failure);
        assert!(f.privileged);
        assert!(f.custom_argv0);
        assert!(f.no_env_expand);
        assert_eq!(rest, "/usr/bin/foo");
    }

    // --- % specifier expansion ---

    #[test]
    fn expand_percent_n() {
        assert_eq!(expand_specifiers("%n", "sshd.service"), "sshd.service");
    }

    #[test]
    fn expand_percent_capital_n() {
        assert_eq!(expand_specifiers("%N", "sshd.service"), "sshd");
    }

    #[test]
    fn expand_percent_p() {
        assert_eq!(expand_specifiers("%p", "sshd.service"), "sshd");
    }

    #[test]
    fn expand_percent_i() {
        assert_eq!(expand_specifiers("%i", "getty@tty1.service"), "tty1");
    }

    #[test]
    fn expand_percent_percent() {
        assert_eq!(expand_specifiers("%%", "foo.service"), "%");
    }

    #[test]
    fn expand_percent_noop() {
        assert_eq!(expand_specifiers("hello world", "foo.service"), "hello world");
    }

    #[test]
    fn expand_percent_unknown_is_literal() {
        assert_eq!(expand_specifiers("%z", "foo.service"), "%z");
    }

    // --- $VAR expansion ---

    #[test]
    fn expand_simple_var() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        assert_eq!(expand_env_in_word("$FOO", &env), "bar");
    }

    #[test]
    fn expand_braced_var() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        assert_eq!(expand_env_in_word("${FOO}", &env), "bar");
    }

    #[test]
    fn expand_dollar_dollar() {
        let env = HashMap::new();
        assert_eq!(expand_env_in_word("$$", &env), "$");
    }

    #[test]
    fn expand_default_value_unset() {
        let env: HashMap<String, String> = HashMap::new();
        assert_eq!(expand_env_in_word("${UNDEF:-default}", &env), "default");
    }

    #[test]
    fn expand_default_value_set() {
        let mut env = HashMap::new();
        env.insert("DEFINED".to_string(), "value".to_string());
        assert_eq!(expand_env_in_word("${DEFINED:-default}", &env), "value");
    }

    #[test]
    fn expand_alternate_value_set() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        assert_eq!(expand_env_in_word("${FOO:+alt}", &env), "alt");
    }

    #[test]
    fn expand_alternate_value_unset() {
        let env: HashMap<String, String> = HashMap::new();
        assert_eq!(expand_env_in_word("${UNDEF:+alt}", &env), "");
    }

    #[test]
    fn expand_inline_var() {
        let mut env = HashMap::new();
        env.insert("PORT".to_string(), "2222".to_string());
        assert_eq!(expand_env_in_word("-p${PORT}", &env), "-p2222");
    }

    #[test]
    fn expand_unset_var_is_empty() {
        let env: HashMap<String, String> = HashMap::new();
        assert_eq!(expand_env_in_word("$UNDEF", &env), "");
    }

    #[test]
    fn expand_standalone_splitting() {
        let mut env = HashMap::new();
        env.insert("OPTS".to_string(), "-o Port=2222 -v".to_string());
        let args = vec!["$OPTS".to_string()];
        let expanded = expand_argv(&args, &env);
        assert_eq!(expanded, vec!["-o", "Port=2222", "-v"]);
    }

    #[test]
    fn expand_standalone_unset_skips() {
        let env: HashMap<String, String> = HashMap::new();
        let args = vec!["$UNDEF".to_string()];
        let expanded = expand_argv(&args, &env);
        let expected: Vec<String> = vec![];
        assert_eq!(expanded, expected);
    }

    #[test]
    fn expand_inline_does_not_split() {
        let mut env = HashMap::new();
        env.insert("PORT".to_string(), "2222 3333".to_string());
        // Inline ${VAR} is expanded in-place, not split.
        let args = vec!["-p${PORT}".to_string()];
        let expanded = expand_argv(&args, &env);
        assert_eq!(expanded, vec!["-p2222 3333"]);
    }

    // --- Full parse_exec_start ---

    #[test]
    fn parse_simple_command() {
        let p = parse_exec_start("/usr/bin/foo bar baz", "test.service").unwrap();
        assert_eq!(p.program, "/usr/bin/foo");
        assert_eq!(p.args, vec!["bar", "baz"]);
        assert!(!p.flags.ignore_failure);
        assert!(!p.flags.via_shell);
    }

    #[test]
    fn parse_with_prefixes() {
        let p = parse_exec_start("-+/usr/bin/foo arg", "test.service").unwrap();
        assert_eq!(p.program, "/usr/bin/foo");
        assert_eq!(p.args, vec!["arg"]);
        assert!(p.flags.ignore_failure);
        assert!(p.flags.privileged);
    }

    #[test]
    fn parse_via_shell() {
        let p = parse_exec_start("|/usr/bin/foo bar", "test.service").unwrap();
        assert_eq!(p.program, "/usr/bin/foo");
        assert!(p.flags.via_shell);
    }

    #[test]
    fn parse_no_env_expand() {
        let p = parse_exec_start(":/usr/bin/foo $VAR", "test.service").unwrap();
        assert!(p.flags.no_env_expand);
    }

    // --- Shell quoting ---

    #[test]
    fn shell_quote_simple() {
        assert_eq!(quote_for_shell("hello"), "'hello'");
    }

    #[test]
    fn shell_quote_with_single_quote() {
        assert_eq!(quote_for_shell("it's"), "'it'\\''s'");
    }

    // --- is_standalone_var_ref ---

    #[test]
    fn standalone_var_ref_simple() {
        assert!(is_standalone_var_ref("$FOO"));
        assert!(is_standalone_var_ref("$FOO_BAR"));
        assert!(is_standalone_var_ref("$FOO1"));
    }

    #[test]
    fn standalone_var_ref_not() {
        assert!(!is_standalone_var_ref("${FOO}"));
        assert!(!is_standalone_var_ref("$"));
        assert!(!is_standalone_var_ref("$"));
        assert!(!is_standalone_var_ref("x$FOO"));
    }

    // --- Empty string ---

    #[test]
    fn parse_empty_fails() {
        assert!(parse_exec_start("", "test.service").is_err());
    }
}
