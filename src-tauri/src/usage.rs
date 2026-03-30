use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::process::Command;

const KIRO_USAGE_TIMEOUT: Duration = Duration::from_secs(20);
const USAGE_ERROR_SNIPPET_MAX_CHARS: usize = 280;

pub async fn query_backend_usage_for_host(
    backend_kind: &str,
    ssh_host: Option<&str>,
) -> Result<Value, String> {
    match backend_kind.trim().to_ascii_lowercase().as_str() {
        "codex" => query_codex_usage(ssh_host).await,
        "kiro" => query_kiro_usage(ssh_host).await,
        "tycode" | "claude" | "gemini" => Err(format!("{backend_kind} does not expose usage limits")),
        other => Err(format!("Unknown backend kind: {other}")),
    }
}

async fn query_codex_usage(ssh_host: Option<&str>) -> Result<Value, String> {
    let captured_at_ms = unix_now_ms();
    let raw = crate::codex::query_account_rate_limits(ssh_host).await?;
    let snapshot = select_codex_snapshot(&raw).ok_or_else(|| {
        let snippet = truncate_chars(&raw.to_string(), USAGE_ERROR_SNIPPET_MAX_CHARS);
        format!("Codex returned no rate-limit snapshot: {snippet}")
    })?;

    let primary = parse_codex_window(snapshot.get("primary"), "primary", "5-hour");
    let secondary = parse_codex_window(snapshot.get("secondary"), "secondary", "Weekly");

    let windows = vec![
        primary.unwrap_or_else(|| build_window("primary", "5-hour", None, None, None, None)),
        secondary.unwrap_or_else(|| build_window("secondary", "Weekly", None, None, None, None)),
    ];

    let mut details = Vec::new();
    if let Some(credits) = snapshot.get("credits").and_then(Value::as_object) {
        if credits
            .get("unlimited")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            details.push("Credits: unlimited".to_string());
        } else if let Some(balance) = credits
            .get("balance")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            details.push(format!("Credit balance: {balance}"));
        }

        if credits
            .get("hasCredits")
            .and_then(Value::as_bool)
            .is_some_and(|has| !has)
        {
            details.push("No plan credits available".to_string());
        }
    }

    Ok(json!({
        "backend_kind": "codex",
        "source": "codex_app_server",
        "captured_at_ms": captured_at_ms,
        "plan": snapshot.get("planType").and_then(Value::as_str),
        "status": Value::Null,
        "windows": windows,
        "details": details,
    }))
}

async fn query_kiro_usage(ssh_host: Option<&str>) -> Result<Value, String> {
    let captured_at_ms = unix_now_ms();
    let output = if let Some(host) = ssh_host {
        let args: Vec<String> = ["chat", "--no-interactive", "/usage"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut child = crate::remote::spawn_remote_process(host, "kiro-cli", &args, None).await?;
        // Close stdin so kiro-cli doesn't wait for input.
        drop(child.stdin.take());
        tokio::time::timeout(KIRO_USAGE_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| {
                "Timed out waiting for remote `kiro-cli chat --no-interactive /usage`".to_string()
            })?
            .map_err(|err| format!("Failed to run remote Kiro usage command: {err}"))?
    } else {
        tokio::time::timeout(
            KIRO_USAGE_TIMEOUT,
            Command::new("kiro-cli")
                .args(["chat", "--no-interactive", "/usage"])
                .output(),
        )
        .await
        .map_err(|_| "Timed out waiting for `kiro-cli chat --no-interactive /usage`".to_string())?
        .map_err(|err| format!("Failed to run Kiro usage command: {err}"))?
    };

    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&output.stdout));
    if !combined.is_empty() {
        combined.push('\n');
    }
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    let clean_capture = strip_terminal_noise(&combined);
    let parsed = parse_kiro_usage_capture(&clean_capture);

    if parsed.percent.is_none() && parsed.credits_used.is_none() && parsed.credits_total.is_none() {
        let snippet = if clean_capture.trim().is_empty() {
            "empty output".to_string()
        } else {
            truncate_chars(
                &clean_capture.trim().replace('\n', " | "),
                USAGE_ERROR_SNIPPET_MAX_CHARS,
            )
        };
        return if output.status.success() {
            Err(format!("Unable to parse Kiro usage output: {snippet}"))
        } else {
            Err(format!("Kiro usage command failed: {snippet}"))
        };
    }

    let mut details = Vec::new();
    if let (Some(used), Some(total)) = (parsed.credits_used, parsed.credits_total) {
        details.push(format!(
            "{} of {} covered in plan",
            format_decimal(used),
            format_decimal(total)
        ));
    }

    let windows = vec![build_window(
        "credits",
        "Credits",
        parsed.percent,
        parsed.reset.clone(),
        None,
        None,
    )];

    Ok(json!({
        "backend_kind": "kiro",
        "source": "kiro_cli_usage",
        "captured_at_ms": captured_at_ms,
        "plan": parsed.plan,
        "status": Value::Null,
        "windows": windows,
        "details": details,
    }))
}
fn select_codex_snapshot(raw: &Value) -> Option<&Value> {
    if let Some(by_limit_id) = raw.get("rateLimitsByLimitId").and_then(Value::as_object) {
        if let Some(snapshot) = by_limit_id.get("codex").filter(|value| value.is_object()) {
            return Some(snapshot);
        }
        if let Some(snapshot) = by_limit_id.values().find(|value| value.is_object()) {
            return Some(snapshot);
        }
    }

    raw.get("rateLimits").filter(|value| value.is_object())
}

fn parse_codex_window(value: Option<&Value>, id: &str, fallback_label: &str) -> Option<Value> {
    let value = value?.as_object()?;
    let used_percent = value
        .get("usedPercent")
        .or_else(|| value.get("used_percent"))
        .and_then(value_to_f64);
    let reset_at_unix = value
        .get("resetsAt")
        .or_else(|| value.get("resets_at"))
        .and_then(|raw| raw.as_i64().or_else(|| raw.as_u64().map(|v| v as i64)));
    let window_minutes = value
        .get("windowDurationMins")
        .or_else(|| value.get("window_duration_mins"))
        .and_then(|raw| raw.as_i64().or_else(|| raw.as_u64().map(|v| v as i64)));
    let label = codex_window_label(window_minutes, fallback_label);
    Some(build_window(
        id,
        &label,
        used_percent,
        None,
        reset_at_unix,
        window_minutes,
    ))
}

fn codex_window_label(window_minutes: Option<i64>, fallback_label: &str) -> String {
    let Some(minutes) = window_minutes else {
        return fallback_label.to_string();
    };

    if minutes >= 10_080 {
        return "Weekly".to_string();
    }

    if minutes >= 60 && minutes % 60 == 0 {
        return format!("{}-hour", minutes / 60);
    }

    format!("{minutes}-min")
}
fn parse_kiro_usage_capture(clean_capture: &str) -> ParsedKiroUsage {
    let mut parsed = ParsedKiroUsage::default();
    let lines: Vec<String> = clean_capture
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();

    for line in &lines {
        let lower = line.to_ascii_lowercase();
        if parsed.plan.is_none() && lower.starts_with("plan") && !lower.contains("covered in plan")
        {
            if let Some(value) = extract_suffix_after_colon(line) {
                if !value.is_empty() {
                    parsed.plan = Some(value);
                }
            }
        }

        if parsed.reset.is_none() && lower.contains("reset") {
            if let Some(value) = extract_suffix_after_colon(line) {
                if !value.is_empty() {
                    parsed.reset = Some(value);
                }
            }
        }

        if parsed.percent.is_none() && lower.contains("credit") {
            parsed.percent = find_first_percent(line);
        }
    }

    if parsed.percent.is_none() {
        parsed.percent = find_first_percent(clean_capture);
    }

    if let Some((used, total)) = extract_kiro_covered_plan_numbers(clean_capture) {
        parsed.credits_used = Some(used);
        parsed.credits_total = Some(total);
        if parsed.percent.is_none() && total > 0.0 {
            parsed.percent = Some((used / total) * 100.0);
        }
    }

    parsed
}

#[derive(Default)]
struct ParsedKiroUsage {
    plan: Option<String>,
    reset: Option<String>,
    percent: Option<f64>,
    credits_used: Option<f64>,
    credits_total: Option<f64>,
}

fn extract_kiro_covered_plan_numbers(input: &str) -> Option<(f64, f64)> {
    let normalized = input.replace(',', "");
    let lower = normalized.to_ascii_lowercase();
    let covered_idx = lower.find("covered in plan")?;
    let prefix = &normalized[..covered_idx];
    let prefix_lower = prefix.to_ascii_lowercase();
    let of_idx = prefix_lower.rfind(" of ")?;

    let left = &prefix[..of_idx];
    let right = &prefix[(of_idx + 4)..];

    let used = extract_numeric_tokens(left)
        .pop()
        .and_then(|token| token.parse::<f64>().ok())?;
    let total = extract_numeric_tokens(right)
        .into_iter()
        .next()
        .and_then(|token| token.parse::<f64>().ok())?;

    Some((used, total))
}

fn extract_numeric_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in input.chars() {
        if ch.is_ascii_digit() || ch == '.' || ch == ',' {
            current.push(ch);
            continue;
        }

        if current.chars().any(|digit| digit.is_ascii_digit()) {
            tokens.push(current.clone());
        }
        current.clear();
    }

    if current.chars().any(|digit| digit.is_ascii_digit()) {
        tokens.push(current);
    }

    tokens
}

fn strip_terminal_noise(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if matches!(chars.peek(), Some('[')) {
                let _ = chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }

        if matches!(ch, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}') {
            continue;
        }
        if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
            continue;
        }
        output.push(ch);
    }
    output
}

fn find_first_percent(input: &str) -> Option<f64> {
    let bytes = input.as_bytes();
    for idx in 0..bytes.len() {
        if bytes[idx] != b'%' {
            continue;
        }
        let mut start = idx;
        while start > 0 {
            let ch = bytes[start - 1] as char;
            if ch.is_ascii_digit() || ch == '.' || ch == ' ' {
                start -= 1;
            } else {
                break;
            }
        }
        let raw = input[start..idx].trim();
        if raw.is_empty() {
            continue;
        }
        if let Ok(value) = raw.parse::<f64>() {
            return Some(value);
        }
    }
    None
}

fn extract_suffix_after_colon(line: &str) -> Option<String> {
    let (_, suffix) = line.split_once(':')?;
    let value = suffix.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn build_window(
    id: &str,
    label: &str,
    used_percent: Option<f64>,
    reset_at_text: Option<String>,
    reset_at_unix: Option<i64>,
    window_minutes: Option<i64>,
) -> Value {
    json!({
        "id": id,
        "label": label,
        "used_percent": used_percent,
        "reset_at_text": reset_at_text,
        "reset_at_unix": reset_at_unix,
        "window_minutes": window_minutes,
    })
}

fn value_to_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|v| v as f64))
        .or_else(|| value.as_u64().map(|v| v as f64))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect::<String>()
}

fn format_decimal(value: f64) -> String {
    let rounded = value.round();
    if (value - rounded).abs() < 1e-6 {
        format!("{}", rounded as i64)
    } else {
        format!("{value:.2}")
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_window_label_formats_common_windows() {
        assert_eq!(codex_window_label(Some(300), "Primary"), "5-hour");
        assert_eq!(codex_window_label(Some(10_080), "Secondary"), "Weekly");
        assert_eq!(codex_window_label(Some(45), "Primary"), "45-min");
    }

    #[test]
    fn extract_kiro_covered_plan_numbers_parses_usage_line() {
        let raw = "Usage: (12.5 of 100 covered in plan)";
        let parsed = extract_kiro_covered_plan_numbers(raw).expect("credits pair");
        assert_eq!(parsed.0, 12.5);
        assert_eq!(parsed.1, 100.0);
    }
}
