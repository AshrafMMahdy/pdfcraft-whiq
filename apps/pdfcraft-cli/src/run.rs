#[cfg(feature = "mcp")]
use super::options::MCP_OPTIONS;
use super::options::{RUN_OPTIONS, flag, positional, validate_options};
#[cfg(feature = "mcp")]
use super::{Arc, Path, Write, automation_args};
use super::{CliError, stdout_line};

// ---- automation --------------------------------------------------------------------------------

pub(crate) fn automation(args: &[String]) -> Result<pdfcraft_automation::Automation, String> {
    let a = pdfcraft_automation::Automation::new();
    match flag(args, "--root") {
        Some(root) => a.with_root(root).map_err(|e| format!("--root {root}: {e}")),
        None => Ok(a),
    }
}

pub(crate) fn tools() -> Result<(), CliError> {
    let list: Vec<serde_json::Value> = pdfcraft_automation::tools()
        .iter()
        .map(|t| serde_json::json!({ "name": t.name, "description": t.description, "read_only": t.read_only, "command": t.command, "input_schema": t.input_schema }))
        .collect();
    stdout_line(format_args!("{}", serde_json::to_string_pretty(&list).unwrap_or_default()))?;
    Ok(())
}

/// Print a tool's result: JSON as JSON; images go to `--out` (or are summarised). The image is
/// written like a tool's own output, so `--root` confines it too.
pub(crate) fn print_output(
    auto: &pdfcraft_automation::Automation,
    content: Vec<pdfcraft_automation::Content>,
    out: Option<&str>,
) -> Result<(), CliError> {
    for c in content {
        match c {
            pdfcraft_automation::Content::Json(v) => stdout_line(format_args!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()))?,
            pdfcraft_automation::Content::Png { data, width, height } => match out {
                Some(path) => {
                    let written = auto.write_output(path, &data).map_err(|e| e.to_string())?;
                    stdout_line(format_args!("{}", serde_json::json!({ "image": written.to_string_lossy(), "width": width, "height": height })))?;
                }
                None => stdout_line(format_args!("{}", png_summary(&data, width, height)))?,
            },
        }
    }
    Ok(())
}

/// What a PNG result is reported as when it isn't written to a file.
fn png_summary(data: &[u8], width: u32, height: u32) -> serde_json::Value {
    serde_json::json!({ "image": "png", "width": width, "height": height, "bytes": data.len(), "hint": "pass --out file.png to save it" })
}

/// A tool's result as JSON values, for the `--report` of a script. Same content as
/// [`print_output`], collected instead of printed.
fn output_values(
    auto: &pdfcraft_automation::Automation,
    content: Vec<pdfcraft_automation::Content>,
    out: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    let mut values = Vec::new();
    for c in content {
        values.push(match c {
            pdfcraft_automation::Content::Json(v) => v,
            pdfcraft_automation::Content::Png { data, width, height } => match out {
                Some(path) => {
                    let written = auto.write_output(path, &data).map_err(|e| e.to_string())?;
                    serde_json::json!({ "image": written.to_string_lossy(), "width": width, "height": height })
                }
                None => png_summary(&data, width, height),
            },
        });
    }
    Ok(values)
}

/// Run a script and print one JSON document: per step its number, tool and `ok`/`error`/`skipped`
/// with the result or the message. The first failing step ends the run, as without `--report`;
/// the steps after it are listed as skipped and the exit status is the same.
fn run_script_report(auto: &mut pdfcraft_automation::Automation, steps: &[serde_json::Value]) -> Result<(), CliError> {
    let mut report = Vec::with_capacity(steps.len());
    let mut failure: Option<String> = None;
    for (i, step) in steps.iter().enumerate() {
        let n = i + 1;
        let tool = step["tool"].as_str();
        if failure.is_some() {
            report.push(serde_json::json!({ "step": n, "tool": tool, "status": "skipped" }));
            continue;
        }
        let outcome = match tool {
            None => Err(format!("step {n}: missing \"tool\"")),
            Some(t) => auto
                .call(t, &step["args"])
                .map_err(|e| e.to_string())
                .and_then(|content| output_values(auto, content, step["out"].as_str()))
                .map_err(|e| format!("step {n} ({t}): {e}")),
        };
        match outcome {
            Ok(result) => report.push(serde_json::json!({ "step": n, "tool": tool, "status": "ok", "result": result })),
            Err(message) => {
                report.push(serde_json::json!({ "step": n, "tool": tool, "status": "error", "error": message }));
                failure = Some(message);
            }
        }
    }
    let document = serde_json::json!({ "ok": failure.is_none(), "steps": report });
    stdout_line(format_args!("{}", serde_json::to_string_pretty(&document).unwrap_or_default()))?;
    failure.map_or(Ok(()), |message| Err(CliError::Message(message)))
}

pub(crate) fn run(args: &[String]) -> Result<(), CliError> {
    validate_options("run", args, RUN_OPTIONS)?;
    let report = args.iter().any(|a| a == "--report");
    if report && flag(args, "--script").is_none() {
        return Err("run: --report needs --script".to_string().into());
    }
    let mut auto = automation(args)?;
    if let Some(script) = flag(args, "--script") {
        let text = std::fs::read_to_string(script).map_err(|e| format!("{script}: {e}"))?;
        let steps: Vec<serde_json::Value> = serde_json::from_str(&text).map_err(|e| format!("{script}: {e}"))?;
        if report {
            return run_script_report(&mut auto, &steps);
        }
        for (i, step) in steps.iter().enumerate() {
            let tool = step["tool"].as_str().ok_or(format!("step {}: missing \"tool\"", i + 1))?;
            let content = auto.call(tool, &step["args"]).map_err(|e| format!("step {} ({tool}): {e}", i + 1))?;
            print_output(&auto, content, step["out"].as_str()).map_err(|e| match e {
                CliError::Message(message) => CliError::Message(format!("step {} ({tool}): {message}", i + 1)),
                stdout => stdout,
            })?;
        }
        return Ok(());
    }
    let tool = *positional(args, RUN_OPTIONS).first().ok_or("run: missing tool name (see `pdfcraft-cli tools`)")?;
    let mut obj = serde_json::Map::new();
    for kv in positional(args, RUN_OPTIONS).iter().skip(1) {
        let (k, v) = kv.split_once('=').ok_or(format!("run: expected key=value, got {kv:?}"))?;
        let value = serde_json::from_str(v).unwrap_or_else(|_| serde_json::Value::String(v.to_string()));
        obj.insert(k.to_string(), value);
    }
    let content = auto.call(tool, &serde_json::Value::Object(obj)).map_err(|e| e.to_string())?;
    print_output(&auto, content, flag(args, "--out"))
}

/// Take `--automation-read-root` / `--automation-write-root` (each `--flag DIR` or `--flag=DIR`,
/// once) out of `args`, and open the roots, which must be existing folders. Returns the arguments
/// that are left. A malformed root flag is an error, never a root silently left out.
#[cfg(feature = "mcp")]
fn automation_roots(command: &str, args: &[String]) -> Result<(Option<Arc<pdfcraft_automation::file_access::AutomationRoots>>, Vec<String>), String> {
    let mut found = automation_args::AutomationArgs::new(None, None);
    let mut left = Vec::new();
    let mut it = args.iter().cloned();
    while let Some(arg) = it.next() {
        if !found.consume(&arg, &mut it).map_err(|e| format!("{command}: {e}"))? {
            left.push(arg);
        }
    }
    let roots = pdfcraft_automation::file_access::AutomationRoots::new(found.read_root.as_deref(), found.write_root.as_deref())
        .map_err(|e| format!("{command}: {e}"))?;
    Ok((roots, left))
}

/// `pdfcraft-cli mcp`: in this fork the White Hat IQ craft-engine server
/// (`pdfcraft_automation::whiq`); `--classic` (or the upstream-only `--compact` / `--root`) runs
/// upstream's tool-table server instead.
#[cfg(feature = "mcp")]
pub(crate) fn mcp(args: &[String]) -> Result<(), CliError> {
    let (roots, args) = automation_roots("mcp", args)?;
    if args.iter().any(|a| a == "--classic" || a == "--compact" || a == "--root" || a.starts_with("--root=")) {
        let args: Vec<String> = args.into_iter().filter(|a| a != "--classic").collect();
        return mcp_classic(roots, &args);
    }
    whiq_mcp(roots, &args)
}

/// The WHIQ server's flags (CONTRACT-PARITY.md §2 names; the older GridCraft spellings too).
#[cfg(feature = "mcp")]
fn whiq_settings(args: &[String]) -> Result<pdfcraft_automation::whiq::Settings, String> {
    let mut s = pdfcraft_automation::whiq::Settings::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = arg.split_once('=').map_or((arg.as_str(), None), |(f, v)| (f, Some(v.to_string())));
        if flag == "--automation-no-old-copies" {
            s.no_old_copies = true;
            continue;
        }
        let mut value = || -> Result<String, String> {
            match &inline {
                Some(v) => Ok(v.clone()),
                None => it.next().cloned().ok_or_else(|| format!("mcp: {flag} needs a value")),
            }
        };
        let num = |v: String| -> Result<u64, String> { v.trim().parse::<u64>().map_err(|_| format!("mcp: {flag} needs a whole number, not {v:?}")) };
        match flag {
            "--author" => s.author = Some(value()?),
            "--busy-after-ms" => s.busy_after_ms = num(value()?)?,
            "--job-after-ms" | "--call-timeout-ms" => s.job_after_ms = num(value()?)?,
            "--open-job-mb" | "--job-open-mb" => {
                let v = value()?;
                let mb: f64 = v.trim().parse().map_err(|_| format!("mcp: {flag} needs a number of MB, not {v:?}"))?;
                if !(mb.is_finite() && mb >= 0.0) {
                    return Err(format!("mcp: {flag} needs a number of MB, not {v:?}"));
                }
                s.open_job_bytes = (mb * 1024.0 * 1024.0) as u64;
            }
            "--idle-evict-secs" | "--evict-idle-secs" => s.idle_evict_secs = Some(num(value()?)?).filter(|n| *n > 0),
            "--max-memory-mb" => s.max_memory_mb = Some(num(value()?)?).filter(|n| *n > 0),
            "--max-result-bytes" => s.max_result_bytes = usize::try_from(num(value()?)?.max(1024)).unwrap_or(usize::MAX),
            "--max-result-kb" => s.max_result_bytes = usize::try_from(num(value()?)?.saturating_mul(1024).max(1024)).unwrap_or(usize::MAX),
            "--preview-max-bytes" => s.preview_max_bytes = usize::try_from(num(value()?)?.max(4096)).unwrap_or(usize::MAX),
            "--checkpoint-edits" => s.checkpoint_edits = u32::try_from(num(value()?)?).unwrap_or(u32::MAX),
            "--checkpoint-secs" => s.checkpoint_secs = num(value()?)?,
            _ => {
                return Err(format!(
                    "mcp: unknown option {arg} (the WHIQ server takes --author, --automation-read-root, --automation-write-root, --automation-no-old-copies, --busy-after-ms, --job-after-ms, --open-job-mb, --idle-evict-secs, --max-memory-mb, --max-result-bytes, --checkpoint-edits, --checkpoint-secs; --classic runs the upstream server)"
                ));
            }
        }
    }
    Ok(s)
}

#[cfg(feature = "mcp")]
fn whiq_mcp(roots: Option<Arc<pdfcraft_automation::file_access::AutomationRoots>>, args: &[String]) -> Result<(), CliError> {
    let mut settings = whiq_settings(args).map_err(CliError::Message)?;
    if let Some(r) = &roots {
        settings.read_root = r.read_root().map(Path::to_path_buf);
        settings.write_root = r.write_root().map(Path::to_path_buf);
    }
    // The server reads back what it writes (a converted PDF it opens, split parts, recovered
    // files): the write root is readable too. Nothing outside the two roots is.
    let roots = match &roots {
        Some(r) => pdfcraft_automation::file_access::AutomationRoots::new_write_readable(r.read_root(), r.write_root()).map_err(CliError::Message)?,
        None => None,
    };
    let _ = writeln!(
        std::io::stderr().lock(),
        "pdfcraft-cli: WHIQ MCP server on stdio (protocol {}); read root {}, write root {}; close stdin to stop",
        pdfcraft_automation::whiq::PROTOCOL_VERSIONS[0],
        settings.read_root.as_deref().map_or_else(|| "none".to_string(), |r| r.display().to_string()),
        settings.write_root.as_deref().map_or_else(|| "none".to_string(), |r| r.display().to_string()),
    );
    let make = move || pdfcraft_automation::Automation::new().with_automation_roots(roots);
    pdfcraft_automation::whiq::serve(make, settings, std::io::stdin().lock(), std::io::stdout()).map_err(|e| CliError::Message(e.to_string()))
}

#[cfg(feature = "mcp")]
fn mcp_classic(roots: Option<Arc<pdfcraft_automation::file_access::AutomationRoots>>, args: &[String]) -> Result<(), CliError> {
    validate_options("mcp", args, MCP_OPTIONS)?;
    let compact = args.iter().any(|a| a == "--compact");
    let mut auto = automation(args)?;
    if let Some(roots) = roots {
        let show = |root: Option<&Path>| root.map_or_else(|| "none".to_string(), |r| r.display().to_string());
        let _ = writeln!(
            std::io::stderr().lock(),
            "pdfcraft-cli: files confined: read root {}, write root {}",
            show(roots.read_root()),
            show(roots.write_root())
        );
        auto = auto.with_automation_roots(Some(roots));
    }
    let mut server = pdfcraft_automation::mcp::McpServer::new(auto).with_compact(compact);
    let _ = writeln!(
        std::io::stderr().lock(),
        "pdfcraft-cli: MCP server on stdio (protocol {}{}); close stdin to stop",
        pdfcraft_automation::mcp::PROTOCOL_VERSIONS[0],
        if compact { ", compact tool list" } else { "" }
    );
    server.serve(std::io::stdin().lock(), std::io::stdout().lock()).map_err(|e| CliError::Message(e.to_string()))
}
