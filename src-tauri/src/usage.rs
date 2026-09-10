//! Usage and status data for the side panel.
//!
//! Two sources, deliberately kept apart:
//!
//! * **Session metrics** come from the file the harness already writes to
//!   `~/.dsh/storages/session_projcache.json`. No network, no credentials, and
//!   the numbers are the harness's own — not an approximation of them.
//! * **Account balance** comes from DeepSeek's `/user/balance`, the only usage
//!   endpoint the public API exposes. There is no public endpoint for request,
//!   token or cost history, so the platform dashboard's charts cannot be
//!   reproduced here; what the panel shows instead is measured locally.
//!
//! The balance is fetched on a background thread and cached, so reading the
//! panel never waits on the network.

use std::{
    env, fs,
    path::PathBuf,
    sync::OnceLock,
    time::{Duration, Instant, SystemTime},
};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;

/// How stale the cached balance may get before it is fetched again.
const BALANCE_TTL: Duration = Duration::from_secs(300);

/// How many session rows the panel is sent.
///
/// It renders the current session plus eight rows, and a busy harness can hold
/// hundreds, so serialising every one of them over IPC every two seconds is
/// waste. The margin covers a render that lands between two polls.
const MAX_SESSIONS: usize = 12;

/// Where the harness keeps its state.
///
/// The harness resolves its own home from `DSH_HOME` first, so the shell has to
/// look in the same place: hardcoding `$HOME/.dsh` shows "no sessions recorded
/// yet" and "no API key found", and disables the session-resume seed, for every
/// user who sets it.
fn harness_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("DSH_HOME").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".dsh"))
}

// ---- session metrics -----------------------------------------------------

#[derive(Serialize, Default, Clone)]
pub struct Tokens {
    pub uncached_input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Serialize, Default, Clone)]
pub struct Context {
    pub surface: u64,
    pub window: u64,
    pub pressure: u64,
}

#[derive(Serialize, Default, Clone)]
pub struct Breakdown {
    pub system: u64,
    pub tools: u64,
    pub messages: u64,
}

#[derive(Serialize, Default, Clone)]
pub struct Stats {
    pub turns: u64,
    pub steps: u64,
    pub llm_ms: u64,
    pub tool_ms: u64,
    pub ttft_ms: u64,
    pub decode_ms: u64,
    pub decode_tokens: u64,
}

#[derive(Serialize, Clone)]
pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub last_prompt_at: Option<f64>,
    pub tokens: Tokens,
    pub context: Context,
    pub breakdown: Breakdown,
    pub stats: Stats,
}

#[derive(Serialize, Clone)]
pub struct Balance {
    pub currency: String,
    pub total: String,
    pub granted: String,
    pub topped_up: String,
    pub available: bool,
}

#[derive(Serialize, Default)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub balance: Option<Balance>,
    /// Why the balance is missing, when it is. Shown in the panel rather than
    /// swallowed, so a wrong key or an offline machine explains itself.
    pub balance_error: Option<String>,
    pub source: Option<String>,
}

/// A row is stored as `{"val": …}`; take the inner value when it is there.
fn row<'a>(rows: &'a Value, name: &str) -> Option<&'a Value> {
    let row = rows.get(name)?;
    Some(row.get("val").unwrap_or(row))
}

fn number(value: Option<&Value>, field: &str) -> u64 {
    value
        .and_then(|v| v.get(field))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Path of the harness's session projection cache.
fn session_cache_path() -> Result<PathBuf, String> {
    Ok(harness_dir()
        .ok_or_else(|| "could not locate the home directory".to_string())?
        .join("storages")
        .join("session_projcache.json"))
}

/// Read every session the harness has cached.
pub fn sessions() -> Result<(Vec<Session>, PathBuf), String> {
    let path = session_cache_path()?;

    let text =
        fs::read_to_string(&path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;

    let table = parsed
        .get("tables")
        .and_then(|t| t.get("sessions"))
        .and_then(Value::as_object)
        .ok_or_else(|| "no sessions recorded yet".to_string())?;

    let mut sessions: Vec<Session> = table
        .iter()
        .filter_map(|(id, entry)| {
            let rows = entry.get("rows")?;

            let usage = row(rows, "tokenUsage");
            let totals = usage.and_then(|u| u.get("totals"));
            let pressure = row(rows, "contextPressure");
            let breakdown = row(rows, "contextBreakdown");
            let stats = row(rows, "sessionStats");
            let metadata = row(rows, "sessionListMetadata");

            Some(Session {
                id: id.trim_start_matches("session-").to_string(),
                title: row(rows, "title")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                last_prompt_at: metadata
                    .and_then(|m| m.get("lastPromptAt"))
                    .and_then(Value::as_f64),
                tokens: Tokens {
                    uncached_input: number(totals, "uncachedInputTokens"),
                    output: number(totals, "outputTokens"),
                    cache_read: number(totals, "cacheReadTokens"),
                    cache_write: number(totals, "cacheWriteTokens"),
                },
                context: Context {
                    surface: number(pressure, "surfaceTokens"),
                    window: number(pressure, "contextWindow"),
                    pressure: number(pressure, "pressureTokens"),
                },
                breakdown: Breakdown {
                    system: number(breakdown, "systemTokens"),
                    tools: number(breakdown, "toolsTokens"),
                    messages: number(breakdown, "messageTokens"),
                },
                stats: Stats {
                    turns: number(stats, "turns"),
                    steps: number(stats, "steps"),
                    llm_ms: number(stats, "llmMs"),
                    tool_ms: number(stats, "toolMs"),
                    ttft_ms: number(stats, "ttftMs"),
                    decode_ms: number(stats, "decodeMs"),
                    decode_tokens: number(stats, "decodeTokens"),
                },
            })
        })
        .collect();

    // Most recently used first; that is the one the user is looking at.
    sessions.sort_by(|a, b| {
        b.last_prompt_at
            .unwrap_or(0.0)
            .total_cmp(&a.last_prompt_at.unwrap_or(0.0))
    });

    Ok((sessions, path))
}

/// The last parse of the session cache, keyed by the file it came from.
struct SessionsMemo {
    path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
    sessions: Vec<Session>,
}

fn sessions_memo() -> &'static std::sync::Mutex<Option<SessionsMemo>> {
    static MEMO: OnceLock<std::sync::Mutex<Option<SessionsMemo>>> = OnceLock::new();
    MEMO.get_or_init(|| std::sync::Mutex::new(None))
}

/// Session rows for the panel, memoized on the cache file's identity.
///
/// The panel polls every two seconds, and re-reading plus re-parsing the whole
/// cache each time spends a blocking thread on numbers that only change when a
/// turn ends. The file's (modified time, length) is the key: the harness
/// rewrites it whenever it records anything, so a mismatch is the signal to
/// re-read. At most [`MAX_SESSIONS`] rows come back, because that is all the
/// panel renders.
pub fn sessions_cached() -> Result<(Vec<Session>, PathBuf), String> {
    let path = session_cache_path()?;
    let identity = fs::metadata(&path).map_or((None, 0), |meta| (meta.modified().ok(), meta.len()));

    if let Ok(guard) = sessions_memo().lock() {
        if let Some(memo) = guard.as_ref() {
            if memo.path == path && (memo.modified, memo.len) == identity {
                return Ok((memo.sessions.clone(), memo.path.clone()));
            }
        }
    }

    let (sessions, path) = sessions()?;
    let sessions: Vec<Session> = sessions.into_iter().take(MAX_SESSIONS).collect();
    if let Ok(mut guard) = sessions_memo().lock() {
        *guard = Some(SessionsMemo {
            path: path.clone(),
            modified: identity.0,
            len: identity.1,
            sessions: sessions.clone(),
        });
    }
    Ok((sessions, path))
}

/// The harness's full id of the most recently used session, if any.///
/// `sessions()` strips the `session-` prefix for display; the seed injected
/// into the webview needs the full id back, so it is reconstructed here.
pub fn latest_session_id() -> Option<String> {
    let (sessions, _) = sessions().ok()?;
    sessions
        .first()
        .map(|session| format!("session-{}", session.id))
}

// ---- balance -------------------------------------------------------------

/// Pull `DEEPSEEK_API_KEY` out of the harness's credentials file.
///
/// The file is two levels of trivial YAML, so it is read directly rather than
/// pulling in a YAML parser for one scalar. The key is used to authenticate a
/// single request and is never copied, logged, or written anywhere.
fn api_key() -> Result<String, String> {
    if let Ok(key) = env::var("DEEPSEEK_API_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Ok(key);
        }
    }

    let path = harness_dir()
        .ok_or_else(|| "could not locate the home directory".to_string())?
        .join(".credentials.yaml");
    let text = fs::read_to_string(&path)
        .map_err(|_| "no DeepSeek API key found; sign in through the harness first".to_string())?;

    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != "DEEPSEEK_API_KEY" {
            continue;
        }
        let value = value.trim().trim_matches(['"', '\'']).to_string();
        if !value.is_empty() {
            return Ok(value);
        }
    }
    Err("no DeepSeek API key found in the harness credentials".to_string())
}

fn request_balance() -> Result<Balance, String> {
    let key = api_key()?;
    let response = ureq::get("https://api.deepseek.com/user/balance")
        .set("Authorization", &format!("Bearer {key}"))
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(401, _) => "the DeepSeek API key was rejected".to_string(),
            ureq::Error::Status(code, _) => format!("DeepSeek returned HTTP {code}"),
            ureq::Error::Transport(t) => format!("could not reach DeepSeek: {t}"),
        })?;

    let body: Value = response
        .into_json()
        .map_err(|e| format!("DeepSeek sent something unreadable: {e}"))?;

    let info = body
        .get("balance_infos")
        .and_then(Value::as_array)
        .and_then(|list| list.first())
        .ok_or_else(|| "DeepSeek reported no balance".to_string())?;

    let field = |name: &str| {
        info.get(name)
            .and_then(Value::as_str)
            .unwrap_or("—")
            .to_string()
    };

    Ok(Balance {
        currency: field("currency"),
        total: field("total_balance"),
        granted: field("granted_balance"),
        topped_up: field("topped_up_balance"),
        available: body
            .get("is_available")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

type CachedBalance = (Instant, Result<Balance, String>);

fn balance_cache() -> &'static Mutex<Option<CachedBalance>> {
    static CACHE: OnceLock<Mutex<Option<CachedBalance>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Serialise balance refreshes: whoever holds this lock performs the request,
/// everyone else waits and then reuses the result.
static BALANCE_FETCH_LOCK: Mutex<()> = Mutex::const_new(());

/// The cached balance, refreshed at most every [`BALANCE_TTL`].
///
/// Runs off the command thread: the network request is handed to Tauri's
/// blocking thread pool, so the UI never waits on DeepSeek.
async fn balance(force: bool) -> Result<Balance, String> {
    if !force {
        if let Some((fetched, cached)) = balance_cache().lock().await.as_ref() {
            if fetched.elapsed() < BALANCE_TTL {
                return cached.clone();
            }
            // Stale: answer with what we have and refresh behind the caller.
            // The panel polls every two seconds and must never wait on a
            // network request; the explicit refresh command is the one that
            // waits for a fresh answer.
            refresh_balance_in_background();
            return cached.clone();
        }
    }
    balance_now(force).await
}

/// Fetch the balance, wait for it, and cache the result — failures included,
/// so a rejected key or an offline machine is not retried every two seconds.
async fn balance_now(force: bool) -> Result<Balance, String> {
    // Single-flight for automatic refreshes. A manual refresh skips the wait
    // and always asks the API again.
    let _guard = if force {
        None
    } else {
        Some(BALANCE_FETCH_LOCK.lock().await)
    };

    if !force {
        if let Some((fetched, cached)) = balance_cache().lock().await.as_ref() {
            if fetched.elapsed() < BALANCE_TTL {
                return cached.clone();
            }
        }
    }

    let fresh = fetch_balance().await;
    *balance_cache().lock().await = Some((Instant::now(), fresh.clone()));
    fresh
}

/// Refresh a stale entry without making the caller wait for it.
fn refresh_balance_in_background() {
    tauri::async_runtime::spawn(async move {
        // One refresh at a time: a poll that lands while one is in flight does
        // nothing rather than queueing another request behind it.
        let Ok(_guard) = BALANCE_FETCH_LOCK.try_lock() else {
            return;
        };
        if let Some((fetched, _)) = balance_cache().lock().await.as_ref() {
            if fetched.elapsed() < BALANCE_TTL {
                return;
            }
        }
        let fresh = fetch_balance().await;
        *balance_cache().lock().await = Some((Instant::now(), fresh));
    });
}

/// The network half, isolated so both paths cache identically.
async fn fetch_balance() -> Result<Balance, String> {
    match tauri::async_runtime::spawn_blocking(request_balance).await {
        Ok(result) => result,
        Err(e) => Err(format!("balance request failed: {e}")),
    }
}

fn snapshot_from(
    sessions: Result<(Vec<Session>, PathBuf), String>,
    balance: Result<Balance, String>,
) -> Snapshot {
    let (sessions, source) = match sessions {
        Ok((sessions, path)) => (sessions, Some(path.display().to_string())),
        Err(_) => (Vec::new(), None),
    };

    let (balance, balance_error) = match balance {
        Ok(balance) => (Some(balance), None),
        Err(message) => (None, Some(message)),
    };

    Snapshot {
        sessions,
        balance,
        balance_error,
        source,
    }
}

// ---- the commands the panel calls ---------------------------------------

/// The label of the webview allowed to read this. The harness page shares the
/// window and therefore the IPC bridge, and it has no business reading the
/// account balance, so the caller is checked rather than trusted.
pub const PANEL_LABEL: &str = "panel";

#[tauri::command]
pub async fn usage_snapshot(webview: tauri::Webview) -> Result<Snapshot, String> {
    if webview.label() != PANEL_LABEL {
        return Err("usage data is only available to the status panel".to_string());
    }

    let sessions = tauri::async_runtime::spawn_blocking(sessions_cached)
        .await
        .map_err(|e| format!("could not read sessions: {e}"))?;
    let balance = balance(false).await;

    Ok(snapshot_from(sessions, balance))
}

/// Force a fresh `/user/balance` request and return the new value.
#[tauri::command]
pub async fn refresh_balance(webview: tauri::Webview) -> Result<Balance, String> {
    if webview.label() != PANEL_LABEL {
        return Err("usage data is only available to the status panel".to_string());
    }
    balance(true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the harness actually writes, trimmed to what is read here.
    const FIXTURE: &str = r#"{
      "tables": { "sessions": {
        "session-old": { "rows": {
          "title": { "val": "Older" },
          "sessionListMetadata": { "val": { "lastPromptAt": 100.0 } },
          "tokenUsage": { "val": { "totals": { "uncachedInputTokens": 1, "outputTokens": 2,
                                               "cacheReadTokens": 3, "cacheWriteTokens": 4 } } }
        }},
        "session-new": { "rows": {
          "title": { "val": "Newer" },
          "sessionListMetadata": { "val": { "lastPromptAt": 900.0 } },
          "tokenUsage": { "val": { "totals": { "uncachedInputTokens": 56861, "outputTokens": 61338,
                                               "cacheReadTokens": 6653440, "cacheWriteTokens": 0 } } },
          "contextPressure": { "val": { "surfaceTokens": 90549, "contextWindow": 1000000,
                                        "pressureTokens": 112151 } },
          "contextBreakdown": { "val": { "systemTokens": 1574, "toolsTokens": 6475,
                                         "messageTokens": 90549 } },
          "sessionStats": { "val": { "turns": 3, "steps": 83, "llmMs": 875772,
                                     "toolMs": 685584, "decodeTokens": 61338 } }
        }}
      }}
    }"#;

    fn parse(text: &str) -> Vec<Session> {
        let parsed: Value = serde_json::from_str(text).expect("fixture parses");
        let table = parsed["tables"]["sessions"].as_object().expect("sessions");
        let mut out: Vec<Session> = table
            .iter()
            .map(|(id, entry)| {
                let rows = &entry["rows"];
                let totals = row(rows, "tokenUsage").and_then(|u| u.get("totals"));
                let pressure = row(rows, "contextPressure");
                let stats = row(rows, "sessionStats");
                Session {
                    id: id.trim_start_matches("session-").to_string(),
                    title: row(rows, "title")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    last_prompt_at: row(rows, "sessionListMetadata")
                        .and_then(|m| m.get("lastPromptAt"))
                        .and_then(Value::as_f64),
                    tokens: Tokens {
                        uncached_input: number(totals, "uncachedInputTokens"),
                        output: number(totals, "outputTokens"),
                        cache_read: number(totals, "cacheReadTokens"),
                        cache_write: number(totals, "cacheWriteTokens"),
                    },
                    context: Context {
                        surface: number(pressure, "surfaceTokens"),
                        window: number(pressure, "contextWindow"),
                        pressure: number(pressure, "pressureTokens"),
                    },
                    breakdown: Breakdown::default(),
                    stats: Stats {
                        turns: number(stats, "turns"),
                        steps: number(stats, "steps"),
                        llm_ms: number(stats, "llmMs"),
                        tool_ms: number(stats, "toolMs"),
                        ttft_ms: 0,
                        decode_ms: 0,
                        decode_tokens: number(stats, "decodeTokens"),
                    },
                }
            })
            .collect();
        out.sort_by(|a, b| {
            b.last_prompt_at
                .unwrap_or(0.0)
                .total_cmp(&a.last_prompt_at.unwrap_or(0.0))
        });
        out
    }

    #[test]
    fn reads_the_numbers_the_harness_records() {
        let newest = &parse(FIXTURE)[0];
        assert_eq!(newest.title.as_deref(), Some("Newer"));
        assert_eq!(newest.tokens.cache_read, 6_653_440);
        assert_eq!(newest.tokens.output, 61_338);
        assert_eq!(newest.context.surface, 90_549);
        assert_eq!(newest.context.window, 1_000_000);
        assert_eq!(newest.stats.steps, 83);
    }

    #[test]
    fn orders_sessions_by_most_recent_prompt() {
        // A map has no order of its own, so the panel would otherwise show an
        // arbitrary session as the current one.
        let sessions = parse(FIXTURE);
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["new", "old"]);
    }

    #[test]
    fn missing_fields_read_as_zero_rather_than_failing() {
        // Sessions that have not run yet carry no metrics at all; the panel
        // should still list them.
        let oldest = &parse(FIXTURE)[1];
        assert_eq!(oldest.context.window, 0);
        assert_eq!(oldest.stats.turns, 0);
        assert_eq!(oldest.tokens.uncached_input, 1);
    }
}

/// Checks against the real machine, not a fixture. Ignored by default: they
/// depend on a harness install, and the balance one makes a network call.
///
///   cargo test --lib -- --ignored --nocapture
#[cfg(test)]
mod live {
    #[test]
    #[ignore]
    fn reads_the_real_session_cache() {
        match super::sessions() {
            Ok((sessions, path)) => {
                println!("{} sessions from {}", sessions.len(), path.display());
                for s in sessions.iter().take(3) {
                    println!(
                        "  {:?} ctx {}/{} · out {} · cacheRead {} · steps {}",
                        s.title.as_deref().unwrap_or("(untitled)"),
                        s.context.surface,
                        s.context.window,
                        s.tokens.output,
                        s.tokens.cache_read,
                        s.stats.steps
                    );
                }
            }
            Err(e) => println!("no session cache: {e}"),
        }
    }

    #[test]
    #[ignore]
    fn fetches_the_real_balance() {
        // Never prints the key, only what the endpoint returned.
        match super::request_balance() {
            Ok(b) => println!(
                "balance: {} {} (granted {}, topped up {}) available={}",
                b.total, b.currency, b.granted, b.topped_up, b.available
            ),
            Err(e) => println!("balance unavailable: {e}"),
        }
    }
}
