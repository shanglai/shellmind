//! Heuristic `StepKind` inference for `sm wrap`.
//!
//! Takes a raw shell command and returns the most specific typed `StepKind` we
//! can recognise. Anything we don't recognise — pipes, subshells, unknown
//! commands, malformed flags — falls back to `ShellRaw` with positional `$N`
//! placeholders for slot substitution at exec time.

use crate::platform::{Arg, HttpMethod, Platform, StepKind};

/// Friendly tag for preview rendering (e.g. "http", "copy", "shell"). Stable
/// strings — used as a label, not as a key.
pub fn infer_kind_label(raw: &str) -> &'static str {
    let tokens = tokenize(raw);
    if tokens.is_empty() { return "shell"; }
    if has_shell_meta(&tokens, raw) { return "shell"; }
    let cmd = basename(&tokens[0]);
    let has_redirect = tokens.iter().any(|t| t == ">" || t == ">>");
    match cmd {
        "curl" | "wget" => "http",
        "cp" => "copy",
        "mv" => "move",
        "rm" => "delete",
        "mkdir" => "mkdir",
        "echo" if has_redirect => "write",
        "echo" => "echo",
        "export" => "setenv",
        _ if is_env_assignment(&tokens[0]) => "setenv",
        _ => "shell",
    }
}

/// Parse `raw` into the most specific `StepKind` we can identify, threading
/// slot assignments so matching tokens become `Arg::Slot(i)`.
pub fn infer_step(raw: &str, slot_assignments: &[(String, String)]) -> StepKind {
    let tokens = tokenize(raw);
    if tokens.is_empty() || has_shell_meta(&tokens, raw) {
        return shell_raw_fallback(raw, slot_assignments);
    }
    let cmd = basename(&tokens[0]);
    let result = match cmd {
        "curl" => parse_curl(&tokens, slot_assignments),
        "wget" => parse_wget(&tokens, slot_assignments),
        "cp" => parse_cp_mv(&tokens, slot_assignments, true),
        "mv" => parse_cp_mv(&tokens, slot_assignments, false),
        "rm" => parse_rm(&tokens, slot_assignments),
        "mkdir" => parse_mkdir(&tokens, slot_assignments),
        "echo" => parse_echo(&tokens, slot_assignments),
        "export" => parse_export(&tokens, slot_assignments),
        _ if is_env_assignment(&tokens[0]) => parse_inline_env(&tokens[0], slot_assignments),
        _ => None,
    };
    result.unwrap_or_else(|| shell_raw_fallback(raw, slot_assignments))
}

// ── Tokenizer ────────────────────────────────────────────────────────────────

/// Split on whitespace, respecting single and double quotes. Single quotes are
/// literal; double quotes honour `\"` and `\\` escapes only.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut current = String::new();
    let mut chars = s.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut quoted_anything = false;

    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' { in_single = false; }
            else { current.push(c); }
        } else if in_double {
            if c == '"' { in_double = false; }
            else if c == '\\' {
                if let Some(&next) = chars.peek() {
                    chars.next();
                    current.push(next);
                }
            } else { current.push(c); }
        } else if c == '\'' {
            in_single = true;
            quoted_anything = true;
        } else if c == '"' {
            in_double = true;
            quoted_anything = true;
        } else if c.is_whitespace() {
            if !current.is_empty() || quoted_anything {
                out.push(std::mem::take(&mut current));
                quoted_anything = false;
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() || quoted_anything {
        out.push(current);
    }
    out
}

/// Returns true if any token is a shell control operator, or if the raw string
/// contains a subshell / backtick pattern we don't want to risk parsing.
fn has_shell_meta(tokens: &[String], raw: &str) -> bool {
    if raw.contains("$(") || raw.contains('`') { return true; }
    tokens.iter().any(|t| matches!(t.as_str(),
        "|" | "||" | "&&" | ";" | "&" | "<" | "<<"
    ))
}

fn basename(s: &str) -> &str {
    s.rsplit(|c: char| c == '/' || c == '\\').next().unwrap_or(s)
}

fn is_env_assignment(s: &str) -> bool {
    if let Some(eq_pos) = s.find('=') {
        if eq_pos == 0 { return false; }
        let key = &s[..eq_pos];
        key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    } else {
        false
    }
}

// ── Arg conversion ───────────────────────────────────────────────────────────

/// If `value` matches a slot token exactly, return `Arg::Slot(i)`. Otherwise
/// emit `Arg::Literal`. Try the value as-is first, then with a leading `@`
/// stripped (curl's "load from file" syntax — the user's slot token usually
/// doesn't include the @).
fn to_arg(value: &str, slots: &[(String, String)]) -> Arg {
    for (i, (token, _)) in slots.iter().enumerate() {
        if value == token { return Arg::slot(i); }
    }
    if let Some(stripped) = value.strip_prefix('@') {
        for (i, (token, _)) in slots.iter().enumerate() {
            if stripped == token { return Arg::slot(i); }
        }
    }
    Arg::literal(value)
}

// ── ShellRaw fallback with $N substitution ───────────────────────────────────

fn shell_raw_fallback(raw: &str, slots: &[(String, String)]) -> StepKind {
    // Substitute slot tokens with $1, $2, ... so Executor::shell_raw can resolve
    // them at exec time. Process in reverse so longer tokens don't get
    // partially clobbered by shorter overlapping ones.
    let mut indexed: Vec<(usize, &str)> = slots.iter()
        .enumerate()
        .map(|(i, (tok, _))| (i, tok.as_str()))
        .collect();
    indexed.sort_by_key(|(_, tok)| std::cmp::Reverse(tok.len()));

    let mut cmd = raw.to_string();
    for (i, tok) in indexed {
        cmd = cmd.replace(tok, &format!("${}", i + 1));
    }
    StepKind::ShellRaw { cmd, platform: Platform::current() }
}

// ── curl / wget ──────────────────────────────────────────────────────────────

fn parse_curl(tokens: &[String], slots: &[(String, String)]) -> Option<StepKind> {
    let mut method = HttpMethod::Get;
    let mut url: Option<String> = None;
    let mut body: Option<String> = None;
    let mut headers: Vec<(String, String)> = vec![];
    let mut output: Option<String> = None;
    let mut i = 1;

    while i < tokens.len() {
        let t = tokens[i].as_str();
        match t {
            "-X" | "--request" => {
                let m = tokens.get(i + 1)?;
                method = match m.to_uppercase().as_str() {
                    "GET" => HttpMethod::Get,
                    "POST" => HttpMethod::Post,
                    "PUT" => HttpMethod::Put,
                    "PATCH" => HttpMethod::Patch,
                    "DELETE" => HttpMethod::Delete,
                    _ => return None,
                };
                i += 2;
            }
            "-H" | "--header" => {
                let h = tokens.get(i + 1)?;
                let (k, v) = h.split_once(':')?;
                headers.push((k.trim().to_string(), v.trim().to_string()));
                i += 2;
            }
            "-d" | "--data" | "--data-raw" | "--data-binary" => {
                let b = tokens.get(i + 1)?;
                body = Some(b.clone());
                i += 2;
            }
            "-o" | "--output" => {
                let o = tokens.get(i + 1)?;
                output = Some(o.clone());
                i += 2;
            }
            // No-arg flags we can safely skip
            "-s" | "--silent" | "-S" | "--show-error" | "-L" | "--location"
            | "-i" | "--include" | "-k" | "--insecure" | "-f" | "--fail"
            | "-v" | "--verbose" | "-I" | "--head" => {
                i += 1;
            }
            t if t.starts_with('-') => {
                // Unknown flag — refuse to guess
                return None;
            }
            _ => {
                if url.is_some() { return None; }
                url = Some(tokens[i].clone());
                i += 1;
            }
        }
    }

    let url = url?;
    Some(StepKind::HttpCall {
        url: to_arg(&url, slots),
        method,
        body: body.map(|b| to_arg(&b, slots)),
        headers,
        output: output.map(|o| to_arg(&o, slots)),
    })
}

fn parse_wget(tokens: &[String], slots: &[(String, String)]) -> Option<StepKind> {
    let mut url: Option<String> = None;
    let mut output: Option<String> = None;
    let mut i = 1;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        match t {
            "-O" | "--output-document" => {
                let o = tokens.get(i + 1)?;
                output = Some(o.clone());
                i += 2;
            }
            "-q" | "--quiet" | "-c" | "--continue" | "-N" | "--timestamping" => {
                i += 1;
            }
            t if t.starts_with('-') => return None,
            _ => {
                if url.is_some() { return None; }
                url = Some(tokens[i].clone());
                i += 1;
            }
        }
    }
    let url = url?;
    Some(StepKind::HttpCall {
        url: to_arg(&url, slots),
        method: HttpMethod::Get,
        body: None,
        headers: vec![],
        output: output.map(|o| to_arg(&o, slots)),
    })
}

// ── cp / mv ──────────────────────────────────────────────────────────────────

/// Both follow `<cmd> [flags] <src> <dst>`. Treats any flag as opaque and
/// requires exactly two positional args.
fn parse_cp_mv(tokens: &[String], slots: &[(String, String)], is_copy: bool) -> Option<StepKind> {
    let positional: Vec<&String> = tokens.iter().skip(1)
        .filter(|t| !t.starts_with('-'))
        .collect();
    if positional.len() != 2 { return None; }
    let src = to_arg(positional[0], slots);
    let dst = to_arg(positional[1], slots);
    Some(if is_copy {
        StepKind::FileCopy { src, dst }
    } else {
        StepKind::FileMove { src, dst }
    })
}

// ── rm ───────────────────────────────────────────────────────────────────────

fn parse_rm(tokens: &[String], slots: &[(String, String)]) -> Option<StepKind> {
    let mut recursive = false;
    let mut target: Option<String> = None;
    for t in tokens.iter().skip(1) {
        if t == "-r" || t == "-R" || t == "-rf" || t == "-fr" || t == "--recursive" {
            recursive = true;
        } else if t.starts_with('-') {
            // -f alone or other flags: tolerate, don't bail
            if t.contains('r') || t.contains('R') { recursive = true; }
        } else if target.is_some() {
            // Multiple targets — let the shell handle it
            return None;
        } else {
            target = Some(t.clone());
        }
    }
    let target = target?;
    Some(StepKind::FileDelete { target: to_arg(&target, slots), recursive })
}

// ── mkdir ────────────────────────────────────────────────────────────────────

fn parse_mkdir(tokens: &[String], slots: &[(String, String)]) -> Option<StepKind> {
    // `mkdir -p` is the default behaviour of our executor anyway, so we don't
    // need to track the flag. Require exactly one positional.
    let positional: Vec<&String> = tokens.iter().skip(1)
        .filter(|t| !t.starts_with('-'))
        .collect();
    if positional.len() != 1 { return None; }
    Some(StepKind::MkDir { path: to_arg(positional[0], slots) })
}

// ── echo (and echo > file) ───────────────────────────────────────────────────

fn parse_echo(tokens: &[String], slots: &[(String, String)]) -> Option<StepKind> {
    // Find redirect token, if any
    let mut redirect_idx: Option<(usize, bool)> = None; // (idx, append?)
    for (i, t) in tokens.iter().enumerate() {
        if t == ">" { redirect_idx = Some((i, false)); break; }
        if t == ">>" { redirect_idx = Some((i, true)); break; }
    }

    match redirect_idx {
        Some((idx, append)) => {
            // Need exactly one path token after the redirect
            let path_token = tokens.get(idx + 1)?;
            if tokens.get(idx + 2).is_some() { return None; }
            let content = tokens[1..idx].join(" ");
            Some(StepKind::WriteFile {
                path: to_arg(path_token, slots),
                content: to_arg(&content, slots),
                append,
            })
        }
        None => {
            let message = tokens[1..].join(" ");
            Some(StepKind::Echo { message: to_arg(&message, slots) })
        }
    }
}

// ── export VAR=val / VAR=val ─────────────────────────────────────────────────

fn parse_export(tokens: &[String], slots: &[(String, String)]) -> Option<StepKind> {
    let assignment = tokens.get(1)?;
    if tokens.get(2).is_some() { return None; } // `export VAR=val cmd` is a chained command
    parse_inline_env(assignment, slots)
}

fn parse_inline_env(token: &str, slots: &[(String, String)]) -> Option<StepKind> {
    let (key, value) = token.split_once('=')?;
    if key.is_empty() { return None; }
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { return None; }
    Some(StepKind::SetEnv {
        key: key.to_string(),
        value: to_arg(value, slots),
    })
}
