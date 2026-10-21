//! Turning Komodo responses into text that reads well in a model's context.

use serde_json::Value;

/// Keep the first `max` characters (for JSON and listings).
pub fn truncate_head(s: String, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s;
    }
    let kept: String = s.chars().take(max).collect();
    format!(
        "{kept}\n\n[output truncated: showed {max} of {total} characters. Narrow the request (filters, names, a smaller page) to see the rest.]"
    )
}

/// Keep the last `max` characters (for logs, where the newest lines matter).
pub fn truncate_tail(s: String, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s;
    }
    let kept: String = s.chars().skip(total - max).collect();
    // Start on a line boundary so the first line isn't half a line.
    let kept = match kept.find('\n') {
        Some(i) if i < 500 => kept[i + 1..].to_string(),
        _ => kept,
    };
    format!(
        "[output truncated: showing the last {} of {total} characters. Use a smaller tail or search terms.]\n\n{kept}",
        kept.chars().count()
    )
}

/// Compact JSON. Models read it fine, and it costs far fewer tokens than pretty JSON.
pub fn json(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Render a Komodo `Log` (what GetStackLog, GetDeploymentLog etc. return).
pub fn log(v: &Value) -> String {
    let mut out = String::new();
    let command = s(v, "command");
    if !command.is_empty() {
        out.push_str(&format!("$ {command}\n"));
    }
    if v.get("success").and_then(Value::as_bool) == Some(false) {
        out.push_str("(command reported failure)\n");
    }
    let stdout = s(v, "stdout");
    let stderr = s(v, "stderr");
    if !stdout.is_empty() {
        out.push_str(stdout);
        if !stdout.ends_with('\n') {
            out.push('\n');
        }
    }
    if !stderr.is_empty() {
        out.push_str("--- stderr ---\n");
        out.push_str(stderr);
        if !stderr.ends_with('\n') {
            out.push('\n');
        }
    }
    if stdout.is_empty() && stderr.is_empty() {
        out.push_str("(no log output)\n");
    }
    out
}

fn version(v: &Value) -> Option<String> {
    let ver = v.get("version")?;
    let n = |k: &str| ver.get(k).and_then(Value::as_i64).unwrap_or(0);
    let (a, b, c) = (n("major"), n("minor"), n("patch"));
    (a + b + c > 0).then(|| format!("{a}.{b}.{c}"))
}

fn target(v: &Value) -> String {
    match v.get("target") {
        Some(t) => format!("{} {}", s(t, "type"), s(t, "id")).trim().to_string(),
        None => String::new(),
    }
}

/// Render a full Komodo `Update` (the record every execute call produces).
pub fn update(v: &Value) -> String {
    let id = crate::komodo::extract_id(v).unwrap_or_default();
    let status = s(v, "status");
    let success = v.get("success").and_then(Value::as_bool).unwrap_or(false);
    let outcome = match (status, success) {
        ("Complete", true) => "succeeded",
        ("Complete", false) => "FAILED",
        ("InProgress", _) => "still in progress",
        ("Queued", _) => "queued",
        _ => status,
    };
    let mut out = format!("Update {id}: {} on {} — {outcome}", s(v, "operation"), target(v));
    if let Some(ver) = version(v) {
        out.push_str(&format!(" (version {ver})"));
    }
    out.push('\n');
    let operator = s(v, "operator");
    if !operator.is_empty() {
        out.push_str(&format!("operator: {operator}\n"));
    }
    if let (Some(start), Some(end)) = (
        v.get("start_ts").and_then(Value::as_i64),
        v.get("end_ts").and_then(Value::as_i64),
    ) {
        out.push_str(&format!("duration: {:.1}s\n", (end - start) as f64 / 1000.0));
    }
    let commit = s(v, "commit_hash");
    if !commit.is_empty() {
        out.push_str(&format!("commit: {commit}\n"));
    }
    if let Some(logs) = v.get("logs").and_then(Value::as_array) {
        for l in logs {
            let ok = l.get("success").and_then(Value::as_bool).unwrap_or(true);
            out.push_str(&format!("\n## {} {}\n", s(l, "stage"), if ok { "✓" } else { "✗ failed" }));
            let command = s(l, "command");
            if !command.is_empty() {
                out.push_str(&format!("$ {command}\n"));
            }
            for (label, key) in [("", "stdout"), ("stderr:\n", "stderr")] {
                let text = s(l, key).trim_end();
                if !text.is_empty() {
                    out.push_str(label);
                    out.push_str(text);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// One line per update for ListUpdates.
pub fn update_list(v: &Value) -> String {
    let updates = v.get("updates").and_then(Value::as_array).cloned().unwrap_or_default();
    if updates.is_empty() {
        return "No updates found.".into();
    }
    let mut out = String::new();
    for u in &updates {
        let ok = match (s(u, "status"), u.get("success").and_then(Value::as_bool)) {
            ("Complete", Some(true)) => "ok",
            ("Complete", _) => "FAILED",
            (other, _) => other,
        };
        let who = match s(u, "username") {
            "" => s(u, "operator"),
            name => name,
        };
        out.push_str(&format!(
            "{} | ts {} | {} | {} | {} | by {}",
            s(u, "id"),
            u.get("start_ts").and_then(Value::as_i64).unwrap_or(0),
            s(u, "operation"),
            target(u),
            ok,
            who
        ));
        if let Some(ver) = version(u) {
            out.push_str(&format!(" | v{ver}"));
        }
        out.push('\n');
    }
    match v.get("next_page").and_then(Value::as_u64) {
        Some(p) => out.push_str(&format!("\n(more available: pass page = {p})")),
        None => out.push_str("\n(no more pages)"),
    }
    out.push_str("\nTimestamps are Unix milliseconds. Use komodo_get_update with an id for full logs.");
    out
}
