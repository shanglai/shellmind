use std::collections::HashMap;
use anyhow::{Context, Result};
use tokio::process::Command;

use super::{Arg, HttpMethod, Platform, StepKind};

#[derive(Debug, Clone)]
pub struct StepResult {
    pub success: bool,
    pub output: Option<String>,
    pub exit_code: Option<i32>,
}

pub struct Executor {
    platform: Platform,
    env_overlay: HashMap<String, String>,
    /// When true, stdio is inherited — steps run transparently in the terminal.
    /// Use for interactive hook execution (sm __exec). False for sm run (captured output).
    pub interactive: bool,
}

impl Executor {
    pub fn new(platform: Platform) -> Self {
        Self { platform, env_overlay: HashMap::new(), interactive: false }
    }

    pub fn interactive(mut self) -> Self {
        self.interactive = true;
        self
    }

    pub async fn run_steps(
        &mut self,
        steps: &[StepKind],
        slots: &[String],
        continue_on_error: bool,
    ) -> Result<Vec<StepResult>> {
        let mut results = Vec::with_capacity(steps.len());
        let mut outputs: Vec<Option<String>> = Vec::with_capacity(steps.len());

        for (i, step) in steps.iter().enumerate() {
            let result = self.run_step(step, slots, &outputs).await;
            match result {
                Ok(r) => {
                    let success = r.success;
                    outputs.push(r.output.clone());
                    results.push(r);
                    if !success && !continue_on_error { break; }
                }
                Err(e) => {
                    tracing::error!("Step {} error: {}", i, e);
                    outputs.push(None);
                    results.push(StepResult { success: false, output: None, exit_code: None });
                    if !continue_on_error { break; }
                }
            }
        }
        Ok(results)
    }

    async fn run_step(
        &mut self,
        step: &StepKind,
        slots: &[String],
        outputs: &[Option<String>],
    ) -> Result<StepResult> {
        match step {
            StepKind::FileCopy { src, dst } => {
                let src = src.resolve(slots, outputs)?;
                let dst = dst.resolve(slots, outputs)?;
                let src_path = std::path::Path::new(&src);
                if src_path.is_dir() {
                    copy_dir_recursive(&src, &dst)?;
                } else {
                    if let Some(p) = std::path::Path::new(&dst).parent() { std::fs::create_dir_all(p)?; }
                    std::fs::copy(&src, &dst)?;
                }
                Ok(ok(None))
            }
            StepKind::FileMove { src, dst } => {
                let src = src.resolve(slots, outputs)?;
                let dst = dst.resolve(slots, outputs)?;
                if let Some(p) = std::path::Path::new(&dst).parent() { std::fs::create_dir_all(p)?; }
                std::fs::rename(&src, &dst)
                    .or_else(|_| { std::fs::copy(&src, &dst).map(|_| ())?; std::fs::remove_file(&src) })?;
                Ok(ok(None))
            }
            StepKind::FileDelete { target, recursive } => {
                let t = target.resolve(slots, outputs)?;
                let p = std::path::Path::new(&t);
                if p.is_dir() {
                    if *recursive { std::fs::remove_dir_all(&t)?; } else { std::fs::remove_dir(&t)?; }
                } else { std::fs::remove_file(&t)?; }
                Ok(ok(None))
            }
            StepKind::MkDir { path } => {
                let p = path.resolve(slots, outputs)?;
                std::fs::create_dir_all(&p)?;
                Ok(ok(None))
            }
            StepKind::ListDir { path, .. } => {
                let p = path.resolve(slots, outputs)?;
                let mut entries = vec![];
                for e in std::fs::read_dir(&p)? {
                    entries.push(e?.file_name().to_string_lossy().to_string());
                }
                entries.sort();
                let out = entries.join("\n");
                println!("{}", out);
                Ok(ok(Some(out)))
            }
            StepKind::ReadFile { path } => {
                let p = path.resolve(slots, outputs)?;
                let c = std::fs::read_to_string(&p)?;
                Ok(ok(Some(c)))
            }
            StepKind::WriteFile { path, content, append } => {
                let p = path.resolve(slots, outputs)?;
                let c = content.resolve(slots, outputs)?;
                if *append {
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new().append(true).create(true).open(&p)?;
                    write!(f, "{}", c)?;
                } else { std::fs::write(&p, &c)?; }
                Ok(ok(None))
            }
            StepKind::DataSample { file, pct, stratified, strat_col, output } => {
                let file = file.resolve(slots, outputs)?;
                let out = output.resolve(slots, outputs)?;
                data_sample(&file, *pct, *stratified, strat_col.as_deref(), &out)?;
                Ok(ok(Some(out)))
            }
            StepKind::DataJoin { left, right, on, output } => {
                let l = left.resolve(slots, outputs)?;
                let r = right.resolve(slots, outputs)?;
                let out = output.resolve(slots, outputs)?;
                data_join(&l, &r, on, &out)?;
                Ok(ok(Some(out)))
            }
            StepKind::HttpCall { url, method, body, headers, output } => {
                let url = url.resolve(slots, outputs)?;
                let body = body.as_ref().map(|b| b.resolve(slots, outputs)).transpose()?;
                let out = output.as_ref().map(|o| o.resolve(slots, outputs)).transpose()?;
                self.http_call(&url, method, body.as_deref(), headers, out.as_deref()).await
            }
            StepKind::SetEnv { key, value } => {
                let v = value.resolve(slots, outputs)?;
                self.env_overlay.insert(key.clone(), v);
                Ok(ok(None))
            }
            StepKind::ShellRaw { cmd, platform } => {
                if *platform != Platform::current() {
                    anyhow::bail!("ShellRaw tagged for {:?}, running on {:?}", platform, Platform::current());
                }
                let resolved = substitute_positional_slots(cmd, slots);
                self.shell_raw(&resolved).await
            }
            StepKind::Echo { message } => {
                let msg = message.resolve(slots, outputs)?;
                println!("{}", msg);
                Ok(ok(Some(msg)))
            }
        }
    }

    async fn http_call(
        &self,
        url: &str,
        _method: &HttpMethod,
        _body: Option<&str>,
        _headers: &[(String, String)],
        _output: Option<&str>,
    ) -> Result<StepResult> {
        // HTTP support requires reqwest — stub returns placeholder
        // TODO: add reqwest when crate environment supports it
        tracing::warn!("HTTP call to {} — reqwest not linked in this build", url);
        Ok(StepResult { success: false, output: None, exit_code: Some(-1) })
    }

    async fn shell_raw(&self, cmd: &str) -> Result<StepResult> {
        let mut command = if cfg!(target_os = "windows") {
            let mut c = Command::new("powershell");
            c.args(["-Command", cmd]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", cmd]);
            c
        };
        for (k, v) in &self.env_overlay { command.env(k, v); }

        if self.interactive {
            // Inherit stdio — output flows directly to the terminal, no buffering
            let status = command.status().await?;
            Ok(StepResult { success: status.success(), output: None, exit_code: status.code() })
        } else {
            let out = command.output().await?;
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            if !stdout.is_empty() { print!("{}", stdout); }
            Ok(StepResult { success: out.status.success(), output: Some(stdout), exit_code: out.status.code() })
        }
    }
}

fn ok(output: Option<String>) -> StepResult {
    StepResult { success: true, output, exit_code: Some(0) }
}

/// Replace `$1`, `$2`, … in a shell-raw command with the positional slot
/// values. Iterates in reverse index order so `$10` is substituted before
/// `$1`, preventing partial matches when more than 9 slots are bound.
fn substitute_positional_slots(cmd: &str, slots: &[String]) -> String {
    let mut indexed: Vec<(usize, &String)> = slots.iter().enumerate().collect();
    indexed.sort_by_key(|(i, _)| std::cmp::Reverse(*i));
    let mut out = cmd.to_string();
    for (i, val) in indexed {
        out = out.replace(&format!("${}", i + 1), val);
    }
    out
}

fn copy_dir_recursive(src: &str, dst: &str) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = std::path::Path::new(dst).join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path().to_string_lossy(), &dst_path.to_string_lossy())?;
        } else {
            std::fs::copy(entry.path(), dst_path)?;
        }
    }
    Ok(())
}

fn data_sample(file: &str, pct: f32, stratified: bool, strat_col: Option<&str>, output: &str) -> Result<()> {
    let content = std::fs::read_to_string(file)?;
    let mut lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() { return Ok(()); }
    let header = lines.remove(0);
    let n = ((lines.len() as f32) * pct).ceil() as usize;

    let sampled: Vec<&str> = if stratified && strat_col.is_some() {
        let cols: Vec<&str> = header.split(',').collect();
        let col_idx = cols.iter().position(|c| *c == strat_col.unwrap())
            .with_context(|| format!("Column '{}' not found", strat_col.unwrap()))?;
        let mut groups: HashMap<&str, Vec<&str>> = HashMap::new();
        for line in &lines {
            let val = line.split(',').nth(col_idx).unwrap_or("");
            groups.entry(val).or_default().push(line);
        }
        let mut result = vec![];
        for (_, group) in &groups {
            let take = ((group.len() as f32) * pct).ceil() as usize;
            result.extend(group.iter().take(take));
        }
        result
    } else {
        let step = (lines.len() as f32 / n as f32).ceil() as usize;
        lines.iter().step_by(step.max(1)).take(n).copied().collect()
    };

    if let Some(p) = std::path::Path::new(output).parent() { std::fs::create_dir_all(p)?; }
    let out = format!("{}\n{}", header, sampled.join("\n"));
    std::fs::write(output, out)?;
    Ok(())
}

fn data_join(left: &str, right: &str, on: &str, output: &str) -> Result<()> {
    let lc = std::fs::read_to_string(left)?;
    let rc = std::fs::read_to_string(right)?;
    let mut ll = lc.lines();
    let mut rl = rc.lines();
    let lh = ll.next().context("Left empty")?;
    let rh = rl.next().context("Right empty")?;
    let lc: Vec<&str> = lh.split(',').collect();
    let rc: Vec<&str> = rh.split(',').collect();
    let li = lc.iter().position(|c| *c == on).with_context(|| format!("Key '{on}' not in left"))?;
    let ri = rc.iter().position(|c| *c == on).with_context(|| format!("Key '{on}' not in right"))?;
    let mut ridx: HashMap<String, String> = HashMap::new();
    for line in rl {
        let key = line.split(',').nth(ri).unwrap_or("").to_string();
        ridx.insert(key, line.to_string());
    }
    let extra: Vec<&str> = rc.iter().enumerate().filter(|(i,_)| *i != ri).map(|(_,c)| *c).collect();
    let mut out = vec![format!("{},{}", lh, extra.join(","))];
    for line in ll {
        let key = line.split(',').nth(li).unwrap_or("").to_string();
        if let Some(rrow) = ridx.get(&key) {
            let rv: Vec<&str> = rrow.split(',').enumerate().filter(|(i,_)| *i != ri).map(|(_,v)| v).collect();
            out.push(format!("{},{}", line, rv.join(",")));
        }
    }
    std::fs::write(output, out.join("\n"))?;
    Ok(())
}
