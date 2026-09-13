//! A log file, written for every run.
//!
//! The shell is a GUI app with no console, so anything it or the backend
//! prints is invisible by default. Diagnosing a problem then means asking the
//! user to relaunch the app from a terminal, which is a poor thing to ask and
//! changes the conditions of the run. Everything goes to a file instead, and
//! the window's menu opens it.
//!
//! Two properties of that file matter beyond "it exists":
//!
//! * it holds whatever the harness printed — prompts, tool output, file paths —
//!   and users attach it to bug reports, so it is created private (`0600`, in a
//!   `0700` directory) and every credential-looking value is masked before it
//!   is written;
//! * masking lives here, in the sink, rather than at the call sites: the ready
//!   line that carries this run's launch token is both logged and forwarded to
//!   the failure report, and a caller that forgot to redact would leak it.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

static LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static PATH: OnceLock<PathBuf> = OnceLock::new();

/// Keys whose value is a credential, matched case-insensitively.
///
/// Deliberately a short list of whole words: `token` must not match
/// `max_tokens`, and a bare `token` with no `=`/`:` after it is prose, not a
/// credential.
const SECRET_KEYS: [&str; 8] = [
    "token",
    "access_token",
    "refresh_token",
    "id_token",
    "api_key",
    "apikey",
    "client_secret",
    "password",
];

/// Start a fresh log at `path`.
///
/// The previous run's log is replaced rather than appended to: a stale tail
/// from an earlier session is worse than no history when someone is trying to
/// work out what just happened.
pub fn init(path: PathBuf) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
        restrict_directory(parent);
    }
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).ok();
    let _ = PATH.set(path);
    let _ = LOG.set(Mutex::new(file));
}

/// Keep the log directory private, on the platforms that have modes.
#[cfg(unix)]
fn restrict_directory(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_directory(_dir: &Path) {}

/// Where the log lives, once `init` has run.
pub fn path() -> Option<&'static PathBuf> {
    PATH.get()
}

/// Append one line, with credential-looking values masked.
///
/// Never fails the caller — a broken log must not take the app down with it.
pub fn line(text: &str) {
    let text = redact(text);

    #[cfg(debug_assertions)]
    eprintln!("{text}");

    if let Some(lock) = LOG.get() {
        if let Ok(mut guard) = lock.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = writeln!(file, "{text}");
                let _ = file.flush();
            }
        }
    }
}

/// Replace every credential value in `text` with `***`.
///
/// One pass over the line: every key occurrence is located on an
/// ASCII-lowercased copy (same byte length, so the offsets line up), each
/// candidate is parsed by [`secret_value_end`], and the surviving value spans
/// are masked in order.
fn redact(text: &str) -> String {
    let lowered = text.to_ascii_lowercase();
    let mut spans: Vec<(usize, usize)> = Vec::new();

    for key in SECRET_KEYS {
        let mut from = 0;
        while let Some(offset) = lowered[from..].find(key) {
            let start = from + offset;
            let end = start + key.len();
            // The key must stand alone: `max_tokens` ends in `tokens`, and a
            // key glued to a word character is part of a longer name.
            let standalone = start == 0
                || !text[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|previous| previous.is_alphanumeric() || previous == '_');
            if standalone {
                if let Some((value_start, value_end)) = secret_value_end(&text[end..]) {
                    // Mask the value only: `token=***` still tells a reader
                    // which key was redacted, and `token=` keeps its shape.
                    spans.push((end + value_start, end + value_end));
                }
            }
            from = end;
            if from >= lowered.len() {
                break;
            }
        }
    }

    if spans.is_empty() {
        return text.to_string();
    }

    spans.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in spans {
        // Overlapping candidates (a shorter key inside a longer one) and empty
        // ranges are already handled; nothing is masked twice.
        if start < cursor || start >= end {
            continue;
        }
        out.push_str(&text[cursor..start]);
        out.push_str("***");
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Locate the value following a credential key, when the key is used as one.
///
/// Accepts `key=value`, `key: value`, `"key": "value"` and the URL-encoded
/// `key%3Dvalue`. The value ends at the first character that cannot be part of
/// a bare token, so a following query parameter or sentence is preserved.
///
/// @param after_key - the text immediately after the key.
/// @returns the value's byte range within `after_key`, or `None` when this
///   occurrence is not an assignment at all.
fn secret_value_end(after_key: &str) -> Option<(usize, usize)> {
    let bytes = after_key.as_bytes();
    let mut index = 0;
    if bytes.first() == Some(&b'"') {
        index += 1;
    }
    while matches!(bytes.get(index), Some(b' ') | Some(b'\t')) {
        index += 1;
    }
    let separator =
        if after_key[index..].starts_with("%3D") || after_key[index..].starts_with("%3d") {
            3
        } else if matches!(bytes.get(index), Some(b'=') | Some(b':')) {
            1
        } else {
            return None;
        };
    index += separator;
    if bytes.get(index) == Some(&b'"') {
        index += 1;
    }
    while matches!(bytes.get(index), Some(b' ') | Some(b'\t')) {
        index += 1;
    }
    let value = &after_key[index..];
    let end = value
        .find(|c: char| {
            c.is_whitespace() || matches!(c, '&' | '#' | ',' | '}' | ')' | '"' | '\'' | ';')
        })
        .unwrap_or(value.len());
    // An empty value holds nothing to hide (`token=` stays readable).
    if end == 0 {
        return None;
    }
    Some((index, index + end))
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn masks_the_launch_token_however_it_is_spelled() {
        // The ready line, as the harness prints it (query form).
        assert_eq!(
            redact("dsh web: http://127.0.0.1:34733/?token=secret (LAN: http://10.0.0.2:34733/?token=secret)"),
            "dsh web: http://127.0.0.1:34733/?token=*** (LAN: http://10.0.0.2:34733/?token=***)"
        );
        // JSON bodies, an access-token query, a url-encoded separator, and a
        // capitalised key are all the same credential.
        assert_eq!(redact("{\"token\":\"abc\"}"), "{\"token\":\"***\"}");
        assert_eq!(redact("access_token=abc&x=1"), "access_token=***&x=1");
        assert_eq!(redact("?token%3Dabc"), "?token%3D***");
        assert_eq!(redact("TOKEN=abc"), "TOKEN=***");
        assert_eq!(redact("api_key: sk-live-123"), "api_key: ***");
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        // Whole words only: these are not credentials.
        assert_eq!(
            redact("max_tokens=4096 tokens=2"),
            "max_tokens=4096 tokens=2"
        );
        assert_eq!(
            redact("the token budget is fine"),
            "the token budget is fine"
        );
        assert_eq!(redact("nothing to hide"), "nothing to hide");
        // Nothing follows the key, so there is nothing to mask.
        assert_eq!(redact("token="), "token=");
    }
}
