use crate::model::{CacheTotals, CacheUsage, ContextUsage, ProviderSnapshot};
use crate::providers::ProviderError;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Read the provider's human-readable active model label from a statusLine
/// payload. The display name is intentionally preferred over the model id so
/// the sidebar stays useful at a glance and does not expose provider-specific
/// implementation identifiers.
pub fn parse_model(value: &Value) -> Option<String> {
  value
    .get("model")
    .and_then(Value::as_object)
    .and_then(|model| {
      model
        .get("display_name")
        .or_else(|| model.get("displayName"))
        .and_then(Value::as_str)
    })
    .map(str::trim)
    .filter(|model| !model.is_empty())
    .map(str::to_string)
    .map(|model| match parse_effort(value) {
      Some(effort) => format!("{model} · {effort}"),
      None => model,
    })
}

/// The reasoning effort, suffixed onto the model the way the session's own
/// statusLine shows it. `high` is the default, so naming it would be noise;
/// a provider without the parameter reports no level at all.
fn parse_effort(value: &Value) -> Option<String> {
  value
    .get("effort")
    .and_then(Value::as_object)
    .and_then(|effort| effort.get("level"))
    .and_then(Value::as_str)
    .map(str::trim)
    .filter(|level| !level.is_empty() && *level != "high")
    .map(str::to_string)
}

pub fn parse_context(
  value: Option<&Value>,
) -> std::result::Result<Option<ContextUsage>, ProviderError> {
  let Some(value) = value.filter(|value| !value.is_null()) else {
    return Ok(None);
  };
  let Some(object) = value.as_object() else {
    return Err(ProviderError::UnsupportedResponse(
      "context_window is not an object".to_string(),
    ));
  };
  let used = object
    .get("used_percentage")
    .or_else(|| object.get("usedPercentage"))
    .and_then(Value::as_f64);
  let remaining = object
    .get("remaining_percentage")
    .or_else(|| object.get("remainingPercentage"))
    .and_then(Value::as_f64);
  let Some(percent) = used.or_else(|| remaining.map(|value| 100.0 - value)) else {
    return Ok(None);
  };
  let cache = parse_cache_usage(object.get("current_usage"));
  let total = |snake: &str, camel: &str| {
    object
      .get(snake)
      .or_else(|| object.get(camel))
      .and_then(Value::as_u64)
  };
  let input = total("total_input_tokens", "totalInputTokens");
  let output = total("total_output_tokens", "totalOutputTokens");
  let window_size = total("context_window_size", "contextWindowSize");
  ContextUsage::new(percent)
    .map(|context| {
      context
        .with_cache(cache)
        .with_totals(input, output, window_size)
    })
    .map(Some)
    .map_err(|error| ProviderError::UnsupportedResponse(error.to_string()))
}

/// The branch checked out where the session is working.
///
/// `workspace.current_dir` follows the session; `cwd` is only where it was
/// launched, which for a worktree flow is usually the wrong tree. Resolved
/// here, at statusLine time, so a refresh never pays for it.
pub fn parse_branch(value: &Value) -> Option<String> {
  let directory = value
    .get("workspace")
    .and_then(Value::as_object)
    .and_then(|workspace| {
      workspace
        .get("current_dir")
        .or_else(|| workspace.get("currentDir"))
    })
    .or_else(|| value.get("cwd"))
    .and_then(Value::as_str)
    .filter(|directory| !directory.is_empty())?;
  read_branch(Path::new(directory))
}

/// Read `HEAD` without spawning git. A linked worktree's `.git` is a file
/// pointing at the real git dir, which is where its own `HEAD` lives — the
/// common case here, so the directory form alone would leave it empty.
fn read_branch(start: &Path) -> Option<String> {
  let mut directory = Some(start);
  while let Some(current) = directory {
    let dot_git = current.join(".git");
    let git_dir = if dot_git.is_dir() {
      Some(dot_git)
    } else if dot_git.is_file() {
      std::fs::read_to_string(&dot_git)
        .ok()
        .and_then(|contents| {
          contents.lines().find_map(|line| {
            line
              .trim()
              .strip_prefix("gitdir:")
              .map(|dir| dir.trim().to_string())
          })
        })
        .map(PathBuf::from)
    } else {
      None
    };
    if let Some(git_dir) = git_dir {
      let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
      let head = head.trim();
      return match head.strip_prefix("ref: refs/heads/") {
        Some(branch) if !branch.is_empty() => Some(branch.to_string()),
        // Detached HEAD prints the short sha rather than nothing, so the row
        // still says where the tree is.
        _ if head.len() >= 7 => Some(head[..7].to_string()),
        _ => None,
      };
    }
    directory = current.parent();
  }
  None
}

fn parse_cache_usage(value: Option<&Value>) -> Option<CacheUsage> {
  let object = value?.as_object()?;
  let has_cache_counters = [
    "cache_read_input_tokens",
    "cacheReadInputTokens",
    "cache_creation_input_tokens",
    "cacheCreationInputTokens",
  ]
  .into_iter()
  .any(|name| object.contains_key(name));
  if !has_cache_counters {
    return None;
  }
  let fresh = token_count(object, "input_tokens", "inputTokens");
  let read = token_count(object, "cache_read_input_tokens", "cacheReadInputTokens");
  let creation = token_count(
    object,
    "cache_creation_input_tokens",
    "cacheCreationInputTokens",
  );
  CacheUsage::from_token_counts(fresh, read, creation)
}

fn token_count(object: &serde_json::Map<String, Value>, snake: &str, camel: &str) -> u64 {
  object
    .get(snake)
    .or_else(|| object.get(camel))
    .and_then(Value::as_u64)
    .unwrap_or_default()
}

/// Accumulate cache counters from the provider session transcript.
///
/// StatusLine's `current_usage` is deliberately a latest-request view. The
/// transcript is the only local source that lets us present a session total,
/// so this function stores a byte offset and reads only appended complete
/// lines after the first call. It never starts a model turn or contacts a
/// provider.
pub fn enrich_cache_session(
  snapshot: &mut ProviderSnapshot,
  statusline: &Value,
  previous_cache: Option<&CacheUsage>,
) {
  let Some(context) = snapshot.context.as_mut() else {
    return;
  };
  let Some(cache) = context.cache.as_mut() else {
    return;
  };
  let Some(session_id) = statusline
    .get("session_id")
    .or_else(|| statusline.get("sessionId"))
    .or_else(|| statusline.get("conversation_id"))
    .or_else(|| statusline.get("conversationId"))
    .and_then(Value::as_str)
    .filter(|value| !value.is_empty())
  else {
    return;
  };
  // Keep the session boundary even when this payload has no transcript
  // path. That prevents a later statusLine update from inheriting another
  // session's cache totals.
  cache.session_id = Some(session_id.to_string());
  let Some(path) = statusline.get("transcript_path").and_then(Value::as_str) else {
    return;
  };

  let matching_previous =
    previous_cache.filter(|previous| previous.session_id.as_deref() == Some(session_id));
  let previous_offset = matching_previous
    .map(|previous| previous.transcript_offset)
    .unwrap_or_default();
  let mut increment_totals: Option<CacheTotals> = None;
  let Some((next_offset, transcript_reset)) = read_transcript_increment(
    Path::new(path),
    if matching_previous.is_some() {
      previous_offset
    } else {
      0
    },
    |line| {
      let Ok(entry) = serde_json::from_str::<Value>(line) else {
        return;
      };
      let Some(usage) = assistant_usage(&entry) else {
        return;
      };
      let fresh = token_count(usage, "input_tokens", "inputTokens");
      let read = token_count(usage, "cache_read_input_tokens", "cacheReadInputTokens");
      let creation = token_count(
        usage,
        "cache_creation_input_tokens",
        "cacheCreationInputTokens",
      );
      if fresh == 0 && read == 0 && creation == 0 {
        return;
      }
      if let Some(existing) = increment_totals.as_mut() {
        existing.add_token_counts(fresh, read, creation);
      } else {
        increment_totals = CacheTotals::from_token_counts(fresh, read, creation);
      }
    },
  ) else {
    return;
  };

  let mut totals = if transcript_reset {
    None
  } else {
    matching_previous.and_then(|previous| previous.session_totals.clone())
  };
  if let Some(increment) = increment_totals {
    if let Some(existing) = totals.as_mut() {
      existing.add_token_counts(
        increment.fresh_input_tokens,
        increment.read_tokens,
        increment.creation_tokens,
      );
    } else {
      totals = Some(increment);
    }
  }

  cache.session_totals = totals;
  cache.transcript_offset = next_offset;
}

/// How far back to look for a permission mode. It is only written when it
/// changes, so the record can be far from the end; a mode older than this tail
/// is left unknown rather than paid for on every turn.
const MODE_LOOKBACK_BYTES: u64 = 1 << 20;

/// The session's permission mode, from its transcript.
///
/// Resolved beside the branch rather than inside the cache pass: that pass
/// returns early whenever it has no cache counters to accumulate, and the
/// mode has to survive those turns. Only the transcript's own
/// `permission-mode` records count — the same key appears inside tool
/// payloads, where it is an argument and not a statement about the session.
pub fn parse_permission_mode(value: &Value) -> Option<String> {
  let path = value.get("transcript_path").and_then(Value::as_str)?;
  last_permission_mode(Path::new(path))
}

fn last_permission_mode(path: &Path) -> Option<String> {
  const MARKER: &str = "\"type\":\"permission-mode\"";
  let mut file = File::open(path).ok()?;
  let length = file.metadata().ok()?.len();
  let start = length.saturating_sub(MODE_LOOKBACK_BYTES);
  file.seek(SeekFrom::Start(start)).ok()?;
  let mut tail = String::new();
  BufReader::new(file).read_to_string(&mut tail).ok()?;
  // Scan for the marker before parsing: only the matching line is worth the
  // JSON, and there are thousands of lines in the window.
  let line = tail.lines().rev().find(|line| line.contains(MARKER))?;
  let entry = serde_json::from_str::<Value>(line).ok()?;
  entry
    .get("permissionMode")
    .or_else(|| entry.get("permission_mode"))
    .and_then(Value::as_str)
    .filter(|mode| !mode.is_empty())
    .map(str::to_string)
}

fn read_transcript_increment<F>(path: &Path, offset: u64, mut on_line: F) -> Option<(u64, bool)>
where
  F: FnMut(&str),
{
  let mut file = File::open(path).ok()?;
  let length = file.metadata().ok()?.len();
  let transcript_reset = offset > length;
  let start = if transcript_reset { 0 } else { offset };
  file.seek(SeekFrom::Start(start)).ok()?;
  let mut reader = BufReader::new(file.take(length - start));
  let mut line = Vec::new();
  let mut next_offset = start;
  loop {
    line.clear();
    let bytes_read = reader.read_until(b'\n', &mut line).ok()?;
    if bytes_read == 0 || line.last() != Some(&b'\n') {
      break;
    }
    next_offset += bytes_read as u64;
    let content = &line[..line.len() - 1];
    on_line(&String::from_utf8_lossy(content));
  }
  Some((next_offset, transcript_reset))
}

fn assistant_usage(entry: &Value) -> Option<&serde_json::Map<String, Value>> {
  if entry.get("type").and_then(Value::as_str) != Some("assistant") {
    return None;
  }
  entry
    .get("message")
    .and_then(Value::as_object)
    .and_then(|message| message.get("usage"))
    .or_else(|| entry.get("usage"))
    .and_then(Value::as_object)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::model::{ContextUsage, Provider, UsageWindow, WindowKind};
  use serde_json::json;
  use std::io::Write;

  fn snapshot_with_cache() -> ProviderSnapshot {
    ProviderSnapshot::new(
      Provider::Claude,
      vec![UsageWindow::new(WindowKind::Weekly, 10.0, None).unwrap()],
      0,
    )
    .with_context(Some(
      ContextUsage::new(24.0)
        .unwrap()
        .with_cache(CacheUsage::from_token_counts(1, 1, 1)),
    ))
  }

  #[test]
  fn model_parser_requires_a_human_readable_display_name() {
    assert_eq!(
      parse_model(&json!({"model": {"id": "claude-sonnet-4"}})),
      None
    );
    assert_eq!(
      parse_model(&json!({
          "model": {"displayName": "Sonnet"}
      })),
      Some("Sonnet".to_string())
    );
  }

  #[test]
  fn accumulates_session_cache_counters_once_per_transcript_offset() {
    let transcript = tempfile::NamedTempFile::new().unwrap();
    let first = br#"{"type":"assistant","timestamp":"2026-08-22T10:00:00Z","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":800,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_1h_input_tokens":100}}}}
{"type":"assistant","timestamp":"2026-08-22T10:01:00Z","message":{"usage":{"input_tokens":50,"cache_read_input_tokens":450,"cache_creation_input_tokens":0}}}
"#;
    std::fs::write(transcript.path(), first).unwrap();
    let statusline = json!({
        "session_id": "session-1",
        "transcript_path": transcript.path(),
    });

    let mut first_snapshot = snapshot_with_cache();
    enrich_cache_session(&mut first_snapshot, &statusline, None);
    let first_cache = first_snapshot.context.unwrap().cache.unwrap();
    let first_totals = first_cache.session_totals.clone().unwrap();
    assert_eq!(first_totals.fresh_input_tokens, 150);
    assert_eq!(first_totals.read_tokens, 1_250);
    assert_eq!(first_totals.creation_tokens, 100);
    assert!(first_cache.ttl_seconds.is_none());
    assert!(first_cache.last_activity_unix.is_none());
    assert!(first_cache.transcript_offset > 0);

    let third = br#"{"type":"assistant","timestamp":"2026-08-22T10:02:00Z","message":{"usage":{"input_tokens":20,"cache_read_input_tokens":180,"cache_creation_input_tokens":0}}}
"#;
    let mut file = std::fs::OpenOptions::new()
      .append(true)
      .open(transcript.path())
      .unwrap();
    file.write_all(third).unwrap();

    let mut second_snapshot = snapshot_with_cache();
    enrich_cache_session(&mut second_snapshot, &statusline, Some(&first_cache));
    let second_cache = second_snapshot.context.unwrap().cache.unwrap();
    let second_totals = second_cache.session_totals.as_ref().unwrap();
    assert_eq!(second_totals.fresh_input_tokens, 170);
    assert_eq!(second_totals.read_tokens, 1_430);
    assert_eq!(second_totals.creation_tokens, 100);

    let mut unchanged_snapshot = snapshot_with_cache();
    enrich_cache_session(&mut unchanged_snapshot, &statusline, Some(&second_cache));
    assert_eq!(
      unchanged_snapshot
        .context
        .unwrap()
        .cache
        .unwrap()
        .session_totals,
      second_cache.session_totals
    );
  }
}
