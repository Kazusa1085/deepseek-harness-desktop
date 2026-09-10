//! Launches the DeepSeek Harness `dsh web` server and discovers its URL.
//!
//! The desktop shell does not reimplement the harness: it starts the real
//! `dsh` CLI (the same command that serves the browser GUI) as a child
//! process, waits for it to announce its loopback URL on stdout, and then
//! points a native webview at that URL. On shutdown it tears the whole
//! process group down so no orphaned server survives the window.

use std::{
    cmp::Reverse,
    env,
    ffi::OsStr,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use tauri::Manager;

/// How long to wait for a locally installed backend to print its URL.
const READY_TIMEOUT: Duration = Duration::from_secs(45);

/// How long the `npx` bootstrap may stay completely silent before we give up.
///
/// The portable build downloads roughly 270 MB on first run. A fixed overall
/// deadline is the wrong tool for that: too short and a slow connection is
/// killed mid-download, too long and a genuinely stuck process hangs for
/// quarter of an hour. Judging by *silence* instead means a download that is
/// still making progress is never interrupted, while a wedged one fails fast.
const BOOTSTRAP_IDLE_TIMEOUT: Duration = Duration::from_secs(150);

/// Absolute ceiling for the bootstrap, however chatty it is.
const BOOTSTRAP_TOTAL_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// How many of the backend's first and last output lines to keep for the
/// error report.
///
/// Both ends matter. A Node crash leads with the cause and follows it with
/// dozens of stack frames, so keeping only the tail throws away the one line
/// that explains anything; keeping only the head misses errors reported at the
/// very end of a run.
const HEAD_LINES: usize = 25;
const TAIL_LINES: usize = 25;

/// Minimum gap between progress lines forwarded to the UI.
///
/// `npm --loglevel=http` emits a line per request, which can burst into the
/// hundreds per second; every one of those would otherwise become a separate
/// webview `eval`.
const PROGRESS_THROTTLE: Duration = Duration::from_millis(90);

/// How long a backend may take to announce itself, and how long it may stay
/// silent while doing so.
#[derive(Clone, Copy)]
struct Budget {
    idle: Duration,
    total: Duration,
}

impl Budget {
    /// An already-installed backend: it prints nothing until the URL, so
    /// silence is the only thing there is to measure.
    const fn local() -> Self {
        Self {
            idle: READY_TIMEOUT,
            total: READY_TIMEOUT,
        }
    }

    /// The `npx` bootstrap, which reports every package it fetches.
    const fn bootstrap() -> Self {
        Self {
            idle: BOOTSTRAP_IDLE_TIMEOUT,
            total: BOOTSTRAP_TOTAL_TIMEOUT,
        }
    }
}

/// The exact prefix of the ready line the web app prints when it is listening.
///
/// The whole line is `dsh web: http://127.0.0.1:<port>/?token=<token>`, with an
/// optional ` (LAN: http://<lan-ip>:<port>/?token=<token>)` suffix. Older
/// harness builds print the same line without the `?token=` query.
const ANNOUNCE_PREFIX: &str = "dsh web: ";

/// Windows process creation flag: run the child without giving it a console.
///
/// `node.exe` is a console application. A GUI process that spawns one gets a
/// console window allocated for it by the OS — a black box that pops up beside
/// the app window and stays for the whole session. Every process this shell
/// starts is a background worker the user should never see.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Executable extensions to try when looking a command up by name.
#[cfg(windows)]
const EXE_EXTS: &[&str] = &[".exe", ".cmd", ".bat", ""];
#[cfg(not(windows))]
const EXE_EXTS: &[&str] = &[""];

/// A live `dsh web` process plus enough state to kill its whole tree later.
pub struct Backend {
    child: Child,
    /// Process id; on unix this doubles as the process-group id because the
    /// child is started as its own group leader.
    pid: u32,
}

impl Backend {
    /// Gracefully stop the server (SIGTERM to the group), then force-kill.
    pub fn shutdown(&mut self) {
        clear_pending();
        // Clear it first: once the group is gone the id could be reused, and a
        // late signal must not take an unrelated process with it.
        #[cfg(unix)]
        BACKEND_PGID.store(0, std::sync::atomic::Ordering::SeqCst);
        terminate_group(self.pid);
        // Give the Cordis host a beat to flush, then hard-kill what remains.
        thread::sleep(Duration::from_millis(600));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        // Backstop: if the app exits without an explicit `shutdown`, make sure
        // the child server does not outlive us.
        clear_pending();
        #[cfg(unix)]
        BACKEND_PGID.store(0, std::sync::atomic::Ordering::SeqCst);
        terminate_group(self.pid);
        let _ = self.child.kill();
    }
}

/// Extract the loopback URL from the ready line, such as
/// `dsh web: http://127.0.0.1:54321/?token=<token>` or
/// `dsh web: http://127.0.0.1:54321 (LAN: http://192.168.1.5:54321)`.
///
/// The URL is returned **verbatim**: the first whitespace-delimited token after
/// the announcement, which also drops the optional ` (LAN: …)` suffix. Passing
/// the printed string through unchanged is the point — the harness mints a
/// fresh `?token=` per run and answers the bare origin with `401 dsh web
/// authentication required`, so rebuilding the URL from its parts (as this used
/// to, keeping only the port digits) breaks the window. Anything the harness
/// adds to that URL later keeps working for the same reason.
fn extract_url(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix(ANNOUNCE_PREFIX)?;
    let candidate = rest.split_whitespace().next()?;
    let url = url::Url::parse(candidate).ok()?;
    if url.scheme() != "http" || url.port().is_none() {
        return None;
    }
    if !matches!(url.host_str(), Some("127.0.0.1") | Some("localhost")) {
        return None;
    }
    Some(candidate.to_string())
}

/// Parse a Node version directory name (`v20.11.0`, `20.11.0`) into a sortable
/// triple. Anything unparseable sorts last as `(0, 0, 0)`.
fn parse_version(name: &str) -> (u32, u32, u32) {
    let mut parts = name.trim_start_matches('v').split('.');
    let mut next = || {
        parts
            .next()
            .and_then(|p| p.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|p| p.parse::<u32>().ok())
            .unwrap_or(0)
    };
    (next(), next(), next())
}

/// `bin` directories of Node installs managed by nvm or fnm, newest first.
///
/// Version managers install outside every system directory, so a packaged app
/// would never find them without looking here explicitly.
fn version_manager_bin_dirs(home: &Path) -> Vec<PathBuf> {
    let roots = [
        home.join(".nvm").join("versions").join("node"),
        home.join(".local")
            .join("share")
            .join("fnm")
            .join("node-versions"),
        home.join("Library")
            .join("Application Support")
            .join("fnm")
            .join("node-versions"),
    ];

    let mut found: Vec<((u32, u32, u32), PathBuf)> = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let version = parse_version(&entry.file_name().to_string_lossy());
            let dir = entry.path();
            // nvm keeps `bin` at the top; fnm nests it under `installation`.
            found.push((version, dir.join("bin")));
            found.push((version, dir.join("installation").join("bin")));
        }
    }

    found.sort_by_key(|(version, _)| Reverse(*version));
    found.into_iter().map(|(_, dir)| dir).collect()
}

/// Directories to search when a tool is not on `PATH`.
///
/// This list exists because of one specific failure: a macOS `.app` launched
/// from Finder inherits launchd's minimal `PATH` (`/usr/bin:/bin:/usr/sbin:
/// /sbin`), which contains no Node installation at all. Without these
/// fallbacks a packaged app would fail to start for every user who installed
/// Node with Homebrew, MacPorts, nvm, fnm, Volta or asdf — which is nearly
/// everyone. The same reasoning applies to Linux desktop launchers.
fn extra_bin_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/opt/local/bin")); // MacPorts
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/usr/bin"));
        dirs.push(PathBuf::from("/snap/bin"));
    }
    #[cfg(windows)]
    {
        if let Some(program_files) = env::var_os("ProgramFiles") {
            dirs.push(Path::new(&program_files).join("nodejs"));
        }
        if let Some(local) = env::var_os("LOCALAPPDATA") {
            dirs.push(Path::new(&local).join("Programs").join("nodejs"));
            dirs.push(Path::new(&local).join("Volta").join("bin"));
        }
        if let Some(appdata) = env::var_os("APPDATA") {
            dirs.push(Path::new(&appdata).join("npm"));
        }
    }

    if let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        dirs.push(home.join(".volta").join("bin"));
        dirs.push(home.join(".asdf").join("shims"));
        dirs.push(home.join(".local").join("bin"));
        dirs.extend(version_manager_bin_dirs(&home));
    }

    dirs
}

/// Strip Windows' extended-length (`\\?\`) prefix from a path.
///
/// `resource_dir()` hands back a verbatim path on Windows. Every Win32 API
/// accepts it, so `is_file()` and the rest of our resolution succeed — but
/// Node does not: given `\\?\C:\...\bin.js` it fails with
/// `EISDIR: lstat 'C:'` and exits before announcing its URL. Anything handed
/// to a child process therefore has to be a plain path.
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        // `\\?\UNC\server\share` is really `\\server\share`.
        if let Some(unc) = rest.strip_prefix("UNC\\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        // `\\?\C:\...` is really `C:\...`.
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// Look a command up on `PATH`, honouring Windows executable extensions.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        if let Some(found) = probe_dir(&dir, name) {
            return Some(found);
        }
    }
    None
}

/// Return `<dir>/<name><ext>` for the first extension that is a real file.
fn probe_dir(dir: &Path, name: &str) -> Option<PathBuf> {
    for ext in EXE_EXTS {
        let candidate = dir.join(format!("{name}{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve a command by name: `PATH` first, then the well-known install
/// directories a GUI launcher does not put on `PATH`.
fn find_tool(name: &str) -> Option<PathBuf> {
    find_on_path(name).or_else(|| extra_bin_dirs().iter().find_map(|dir| probe_dir(dir, name)))
}

/// Locate the Node interpreter.
///
/// An explicit `DSH_DESKTOP_NODE` always wins, so a portable build can still
/// be pointed at a different interpreter. Otherwise the one unpacked from the
/// binary is preferred over whatever is installed, because it is the version
/// this release was tested against.
fn find_node(bundled: Option<&Path>) -> Option<PathBuf> {
    if let Some(explicit) = non_empty_env("DSH_DESKTOP_NODE") {
        return Some(PathBuf::from(explicit));
    }
    bundled.map(Path::to_path_buf).or_else(|| find_tool("node"))
}

/// Read an environment variable, treating blank values as unset.
fn non_empty_env(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Does this path look like a JavaScript entry point rather than a binary?
fn is_js_entry(path: &str) -> bool {
    [".js", ".mjs", ".cjs"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// Prepend a directory to the child's `PATH`.
///
/// The harness spawns its own tool subprocesses (`npm`, `npx`, language
/// servers) which expect to find Node the same way we did. Handing them the
/// resolved directory keeps them working under a Finder-launched app.
fn prepend_path(cmd: &mut Command, dir: &Path) {
    let mut dirs = vec![dir.to_path_buf()];
    if let Some(existing) = env::var_os("PATH") {
        dirs.extend(env::split_paths(&existing));
    }
    if let Ok(joined) = env::join_paths(dirs) {
        cmd.env("PATH", joined);
    }
}

/// Is this a Windows batch script rather than a real executable?
///
/// It matters because `npx` ships as `npx.cmd`: `CreateProcess` refuses batch
/// files outright, so spawning one directly fails with "not a valid Win32
/// application". They have to go through `cmd.exe /C`.
#[cfg(windows)]
fn is_batch_script(program: &Path) -> bool {
    program
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
}

/// Build a `Command` for an executable, exposing its own directory to the
/// child so sibling tools remain reachable.
fn command_at(program: &Path) -> Command {
    #[cfg(windows)]
    let mut cmd = if is_batch_script(program) {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(program);
        cmd
    } else {
        Command::new(program)
    };
    #[cfg(not(windows))]
    let mut cmd = Command::new(program);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    if let Some(dir) = program.parent() {
        if !dir.as_os_str().is_empty() {
            prepend_path(&mut cmd, dir);
        }
    }
    cmd
}

/// Append the `web --no-open --port <port>` arguments to a command.
fn add_web_args(cmd: &mut Command) {
    let port = non_empty_env("DSH_DESKTOP_PORT").unwrap_or_else(|| "0".to_string());
    cmd.arg("web").arg("--no-open").arg("--port").arg(port);
}

/// Resolve the backend command to run, in order of preference:
///
/// 1. `DSH_DESKTOP_BACKEND` — explicit path to a `dsh` executable or to a
///    `.js`/`.mjs`/`.cjs` entry run through `node`.
/// 2. A bundled backend under `<root>/backend/node_modules/@deepseek-ai/dsh`.
/// 3. `dsh` on `PATH` (or in a well-known install directory).
/// 4. `npx --yes @deepseek-ai/dsh@latest`.
fn build_command(
    search_roots: &[PathBuf],
    bundled_node: Option<&Path>,
) -> Result<(Command, Budget), String> {
    let node = find_node(bundled_node);

    if let Some(backend) = non_empty_env("DSH_DESKTOP_BACKEND") {
        let mut cmd = if is_js_entry(&backend) {
            let node = node.ok_or_else(|| missing_node_message("DSH_DESKTOP_BACKEND"))?;
            let mut cmd = command_at(&node);
            cmd.arg(&backend);
            cmd
        } else {
            command_at(Path::new(&backend))
        };
        add_web_args(&mut cmd);
        return Ok((cmd, Budget::local()));
    }

    if let Some(node) = node.as_ref() {
        for root in search_roots {
            let bin = root
                .join("backend")
                .join("node_modules")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib")
                .join("bin.js");
            if bin.is_file() {
                let mut cmd = command_at(node);
                cmd.arg(bin);
                add_web_args(&mut cmd);
                return Ok((cmd, Budget::local()));
            }
        }
    }

    if let Some(dsh) = find_tool("dsh") {
        let mut cmd = command_at(&dsh);
        add_web_args(&mut cmd);
        return Ok((cmd, Budget::local()));
    }

    // Last resort: `npx` fetches the harness before it can serve anything, so
    // it gets the longer budget. The portable build does NOT reach this path —
    // it prepends its unpacked runtime to `search_roots`, which is found above.
    if let Some(npx) = find_tool("npx") {
        let mut cmd = command_at(&npx);
        cmd.args(["--yes", "@deepseek-ai/dsh@latest"]);
        // Without this npm is entirely silent while it downloads, which leaves
        // the user staring at a frozen window with no way to tell a slow
        // connection from a wedged process. At `http` it reports every package
        // it fetches, which is what the splash window shows.
        cmd.env("npm_config_loglevel", "http");
        add_web_args(&mut cmd);
        return Ok((cmd, Budget::bootstrap()));
    }

    if node.is_none() {
        return Err(missing_node_message("the bundled backend"));
    }

    Err(
        "no DeepSeek Harness backend found: set DSH_DESKTOP_BACKEND, run `npm run backend:install`, \
         or install Node.js and `dsh`"
            .to_string(),
    )
}

/// The diagnostic shown when Node cannot be located anywhere.
fn missing_node_message(needed_by: &str) -> String {
    format!(
        "Node.js not found, which {needed_by} requires. Install Node.js 22.15 or \
         newer, or set DSH_DESKTOP_NODE to the full path of the `node` binary."
    )
}

/// The backend's process-group id, readable from a signal handler.
///
/// A signal handler may only call async-signal-safe functions, which rules out
/// locking the managed state the rest of the shutdown path uses. An atomic
/// holds the one number the handler needs.
#[cfg(unix)]
static BACKEND_PGID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Windows equivalent of the pending pid: the shell has no signal handler
/// there, so early-window-close cleanup reads this instead.
#[cfg(windows)]
static PENDING_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Remember the just-spawned child before it has announced its URL, so an
/// early window close or a signal can still take it down.
pub fn mark_pending(pid: u32) {
    #[cfg(unix)]
    BACKEND_PGID.store(pid as i32, std::sync::atomic::Ordering::SeqCst);
    #[cfg(windows)]
    PENDING_PID.store(pid, std::sync::atomic::Ordering::SeqCst);
}

/// The child is now owned by `BackendState`; only the Windows fallback needs
/// clearing, because on unix the pgid doubles as the signal-handler kill
/// target until `Backend::shutdown` clears it.
pub fn clear_pending() {
    #[cfg(windows)]
    PENDING_PID.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// Kill a spawned-but-not-yet-registered backend. Called from `RunEvent::Exit`
/// after the managed state has been drained, so the two paths never overlap.
pub fn kill_pending() {
    #[cfg(unix)]
    {
        let pgid = BACKEND_PGID.swap(0, std::sync::atomic::Ordering::SeqCst);
        if pgid > 0 {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        let pid = PENDING_PID.swap(0, std::sync::atomic::Ordering::SeqCst);
        if pid > 0 {
            terminate_group(pid);
        }
    }
}

/// Kill the backend from a signal handler, then leave immediately.
///
/// Only `kill` and `_exit` are called, both async-signal-safe. `_exit` skips
/// destructors deliberately: the group is already gone, and running arbitrary
/// cleanup from a handler is how a tidy shutdown becomes a deadlock.
#[cfg(unix)]
extern "C" fn handle_termination(signal: i32) {
    let pgid = BACKEND_PGID.load(std::sync::atomic::Ordering::SeqCst);
    if pgid > 0 {
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    unsafe {
        libc::_exit(128 + signal);
    }
}

/// Take the backend down when the app is signalled rather than closed.
///
/// Closing the window runs `RunEvent::Exit` and everything is torn down
/// properly. A signal runs neither that nor `Drop`, so `kill`, a logout, or a
/// crashing session manager used to leave the harness — and every tool
/// subprocess it had spawned — running with no window to stop them.
///
/// `SIGKILL` still cannot be caught; nothing can be done about that one.
#[cfg(unix)]
pub fn install_signal_handlers() {
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(
                signal,
                handle_termination as *const () as libc::sighandler_t,
            );
        }
    }
}

#[cfg(windows)]
pub fn install_signal_handlers() {
    // Windows has no equivalent path: the app is a GUI process with no
    // console, so it is closed through its window, which runs the normal
    // shutdown. `taskkill /T` takes the tree down by itself.
}

/// Kill a process group (unix) or a process tree (Windows).
#[cfg(unix)]
fn terminate_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
}

#[cfg(windows)]
fn terminate_group(pid: u32) {
    use std::os::windows::process::CommandExt;
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        // taskkill is a console program too: without this it flashes a window
        // on the way out.
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

/// Render a command as a readable string for diagnostics.
fn describe(cmd: &Command) -> String {
    let args: Vec<String> = cmd
        .get_args()
        .map(OsStr::to_string_lossy)
        .map(|arg| arg.into_owned())
        .collect();
    format!("{} {}", cmd.get_program().to_string_lossy(), args.join(" "))
}

/// One thing the backend told us while starting up.
enum Event {
    /// The loopback URL the server announced — startup is done.
    Url(String),
    /// A line of output, forwarded to the UI and kept for error reports.
    Line(String),
    /// One of the two output streams reached end of file.
    Closed,
}

/// Forward a stream's lines to the log, and to the event channel while
/// startup is still waiting on it.
///
/// Two things here are load-bearing:
///
/// * stdout and stderr get a thread each. Draining them in sequence
///   deadlocks — whichever is read second fills its 64 KB pipe buffer and
///   blocks the child forever.
/// * the loop runs to EOF and **never returns early**. Once startup finishes
///   nobody is listening on the channel any more, and stopping there would
///   leave the pipes unread: the backend would block on its next 64 KB of
///   output, mid-session, long after everything looked healthy.
///
/// The second point is why the stream is read as bytes: `BufRead::lines()`
/// yields `Err(InvalidData)` for a line that is not valid UTF-8, and the
/// `map_while(Result::ok)` that used to be here ended the loop on the first
/// such byte — one binary write from any tool the harness spawns would stop
/// the drain for the rest of the session. Decoding lossily keeps it running.
fn drain<R: Read + Send + 'static>(stream: R, tx: mpsc::Sender<Event>, scan_for_url: bool) {
    thread::spawn(move || {
        let mut listening = true;
        read_lines_lossy(stream, |line| {
            crate::logging::line(&format!("[dsh web] {line}"));

            if !listening {
                return;
            }
            if scan_for_url {
                if let Some(url) = extract_url(line) {
                    let _ = tx.send(Event::Url(url));
                    return;
                }
            }
            if tx.send(Event::Line(line.to_string())).is_err() {
                listening = false;
            }
        });
        let _ = tx.send(Event::Closed);
    });
}

/// Feed one stream's lines to `on_line`, decoding each one lossily.
///
/// Reading bytes rather than `String`s is the point: invalid UTF-8 inside a
/// line becomes U+FFFD instead of an error, so no single bad byte can end the
/// loop. Only a genuine I/O error stops it — the caller still gets its
/// `Closed` event.
fn read_lines_lossy<R: Read>(reader: R, mut on_line: impl FnMut(&str)) {
    let mut reader = BufReader::new(reader);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let text = String::from_utf8_lossy(&buffer);
        on_line(text.trim_end_matches(['\n', '\r']));
    }
}

/// Start the backend and block until it announces its URL.
///
/// `on_progress` receives the backend's output as it arrives so the caller can
/// show it. It is throttled, because `npm --loglevel=http` can burst into
/// hundreds of lines a second.
pub fn spawn_backend(
    search_roots: &[PathBuf],
    on_progress: impl Fn(&str),
) -> Result<(Backend, String), String> {
    // The portable build carries its runtime inside the binary. Unpacking it
    // first means resolution finds a complete Node + harness pair and never
    // reaches the `npx` bootstrap.
    #[cfg(feature = "portable")]
    let (search_roots, bundled_node) = {
        let runtime = crate::portable::prepare(&on_progress)?;
        let node = crate::portable::node(&runtime);
        let mut roots = vec![runtime];
        roots.extend_from_slice(search_roots);
        (roots, node)
    };
    // Both branches yield an owned Vec so the call below is identical either
    // way; the copy is a handful of paths at startup.
    #[cfg(not(feature = "portable"))]
    let (search_roots, bundled_node) = (search_roots.to_vec(), None::<PathBuf>);

    let (mut cmd, budget) = build_command(&search_roots, bundled_node.as_deref())?;
    let description = describe(&cmd);

    // The splash log opens with the one line that explains everything else:
    // the exact command being run. It is throttled like any other progress
    // line, and the log file records it too.
    crate::logging::line(&format!("dsh-desktop: running {description}"));
    on_progress(&format!("$ {description}"));

    // Run the server as its own process-group leader so `terminate_group` can
    // reap the bash/pwsh tool subprocesses it spawns later.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    // stdin is deliberately closed. A shell launched from a terminal inherits
    // that terminal's stdin, which hands the harness — and every tool it
    // spawns — the user's keystrokes, and makes TTY-dependent behaviour differ
    // between a packaged launch and a source launch.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        format!("failed to start the DeepSeek Harness backend ({description}): {e}")
    })?;
    let pid = child.id();
    // The child is live from this moment, before it announces its URL. Register
    // it so an early window close or signal can still kill the whole tree.
    mark_pending(pid);

    let (tx, rx) = mpsc::channel::<Event>();
    if let Some(stdout) = child.stdout.take() {
        drain(stdout, tx.clone(), true);
    }
    if let Some(stderr) = child.stderr.take() {
        drain(stderr, tx.clone(), false);
    }
    // The loop below ends when every sender is gone, so ours must not linger.
    drop(tx);

    let started = Instant::now();
    let mut head: Vec<String> = Vec::new();
    let mut tail: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    let mut last_forwarded: Option<Instant> = None;
    let mut closed = 0usize;

    let outcome = loop {
        match rx.recv_timeout(budget.idle) {
            Ok(Event::Url(url)) => break Ok(url),
            Ok(Event::Line(line)) => {
                // Throttle by time, not by count, so a slow trickle is still
                // shown promptly while a burst is thinned out.
                let due = last_forwarded.is_none_or(|at| at.elapsed() >= PROGRESS_THROTTLE);
                if due {
                    on_progress(&line);
                    last_forwarded = Some(Instant::now());
                }

                if head.len() < HEAD_LINES {
                    head.push(line);
                } else {
                    if tail.len() == TAIL_LINES {
                        tail.remove(0);
                        dropped += 1;
                    }
                    tail.push(line);
                }
            }
            Ok(Event::Closed) => {
                closed += 1;
                if closed == 2 {
                    break Err(format!(
                        "backend exited before announcing its URL ({description})"
                    ));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                break Err(format!(
                    "backend produced no output for {}s and never announced its URL ({description})",
                    budget.idle.as_secs()
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(format!(
                    "backend exited before announcing its URL ({description})"
                ));
            }
        }

        if started.elapsed() > budget.total {
            break Err(format!(
                "backend did not become ready within {}s ({description})",
                budget.total.as_secs()
            ));
        }
    };

    match outcome {
        Ok(url) => {
            // `mark_pending` already published the pgid/pid, so signals and an
            // early-exit kill stay effective until `BackendState` takes over.
            Ok((Backend { child, pid }, url))
        }
        Err(reason) => {
            clear_pending();
            #[cfg(unix)]
            BACKEND_PGID.store(0, std::sync::atomic::Ordering::SeqCst);
            terminate_group(pid);
            let _ = child.kill();
            let _ = child.wait();
            let mut report = head;
            if dropped > 0 {
                report.push(format!("… {dropped} more lines …"));
            }
            report.extend(tail);
            if report.is_empty() {
                Err(reason)
            } else {
                Err(format!("{reason}\n  {}", report.join("\n  ")))
            }
        }
    }
}

/// Search roots where a bundled backend may live: the packaged resource dir,
/// the development project root (found by walking up from the executable),
/// the current working directory, and the cargo manifest dir.
pub fn search_roots(app: &tauri::App) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    // Packaged builds: the resource dir holds the bundled backend.
    if let Ok(res) = app.path().resource_dir() {
        roots.push(res);
    }

    // Development: walk up from the executable to find the project root.
    if let Ok(exe) = env::current_exe() {
        let mut dir = exe.parent().map(Path::to_path_buf);
        for _ in 0..6 {
            match dir {
                Some(d) => {
                    roots.push(d.clone());
                    dir = d.parent().map(Path::to_path_buf);
                }
                None => break,
            }
        }
    }

    // Running from the project root.
    if let Ok(cwd) = env::current_dir() {
        roots.push(cwd);
    }

    // Cargo dev builds: the manifest dir's parent is the project root.
    if let Ok(dir) = env::var("CARGO_MANIFEST_DIR") {
        roots.push(Path::new(&dir).join(".."));
    }

    // Normalise here rather than at the point of use, so every path derived
    // from a root is already safe to hand to Node.
    roots
        .iter()
        .map(|root| strip_verbatim_prefix(root))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{extract_url, is_js_entry, parse_version, read_lines_lossy, strip_verbatim_prefix};
    use std::path::{Path, PathBuf};

    #[test]
    fn extracts_announced_loopback_url() {
        assert_eq!(
            extract_url("dsh web: http://127.0.0.1:54321"),
            Some("http://127.0.0.1:54321".to_string())
        );
    }

    #[test]
    fn keeps_the_launch_token_query() {
        // The bug this guards: the shell rebuilt the URL from the port digits
        // alone, so the window loaded an unauthenticated origin and the server
        // answered `401 dsh web authentication required`.
        let line =
            "dsh web: http://127.0.0.1:34733/?token=iHuI6D4zG02IcjGdcOQ36ujTCX6gDGMvZBUgCwPIjXE";
        let expected = "http://127.0.0.1:34733/?token=iHuI6D4zG02IcjGdcOQ36ujTCX6gDGMvZBUgCwPIjXE";
        assert_eq!(extract_url(line), Some(expected.to_string()));
    }

    #[test]
    fn extracts_url_with_lan_suffix() {
        // The LAN URL is a separate whitespace-delimited token, so it is
        // dropped without touching the token query of the loopback URL.
        assert_eq!(
            extract_url(
                "dsh web: http://127.0.0.1:50340/?token=abc (LAN: http://192.168.1.5:50340/?token=abc)"
            ),
            Some("http://127.0.0.1:50340/?token=abc".to_string())
        );
    }

    #[test]
    fn ignores_lines_that_are_not_the_announcement() {
        assert_eq!(extract_url("listening on 127.0.0.1"), None);
        assert_eq!(extract_url(""), None);
        assert_eq!(
            extract_url("dsh web: opening the default browser; pass --no-open to disable"),
            None
        );
        // Only our own loopback http origin is loaded into the webview.
        assert_eq!(extract_url("dsh web: http://0.0.0.0:34733/?token=abc"), None);
        assert_eq!(extract_url("dsh web: https://127.0.0.1:34733/?token=abc"), None);
        assert_eq!(extract_url("dsh web: http://127.0.0.1/?token=abc"), None);
    }

    #[test]
    fn keeps_draining_after_a_non_utf8_byte() {
        // The bug this guards: `lines().map_while(Result::ok)` ended the loop
        // on the first invalid UTF-8 byte, so one binary write from a tool the
        // harness spawned left that pipe unread for the rest of the session.
        let input: &[u8] = b"first\n\xff\xfe binary\r\n dsh web: http://127.0.0.1:1\nlast";
        let mut lines: Vec<String> = Vec::new();
        read_lines_lossy(input, |line| lines.push(line.to_string()));

        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "first");
        // The binary line survives as replacement characters instead of
        // aborting the drain, and the ready line behind it is still found.
        assert!(lines[1].ends_with(" binary"));
        assert_eq!(
            extract_url(lines[2].trim_start()),
            Some("http://127.0.0.1:1".to_string())
        );
        assert_eq!(lines[3], "last");
    }

    #[test]
    fn parses_node_version_directory_names() {
        assert_eq!(parse_version("v20.11.0"), (20, 11, 0));
        assert_eq!(parse_version("22.3.1"), (22, 3, 1));
        assert_eq!(parse_version("v18"), (18, 0, 0));
        assert_eq!(parse_version("not-a-version"), (0, 0, 0));
    }

    #[test]
    fn orders_node_versions_numerically_not_lexically() {
        // The bug this guards: "v9" sorts after "v20" as a string.
        let mut names = ["v9.0.0", "v20.11.0", "v18.19.1"];
        names.sort_by_key(|n| std::cmp::Reverse(parse_version(n)));
        assert_eq!(names, ["v20.11.0", "v18.19.1", "v9.0.0"]);
    }

    #[test]
    fn strips_the_windows_verbatim_prefix() {
        // The bug this guards: Node fails with `EISDIR: lstat 'C:'` when it is
        // handed a `\\?\` path, so a packaged Windows build never started.
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\C:\Users\max\app\bin.js")),
            PathBuf::from(r"C:\Users\max\app\bin.js")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\UNC\server\share\app")),
            PathBuf::from(r"\\server\share\app")
        );
    }

    #[test]
    fn leaves_ordinary_paths_untouched() {
        assert_eq!(
            strip_verbatim_prefix(Path::new("/usr/local/bin/node")),
            PathBuf::from("/usr/local/bin/node")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"C:\Program Files\nodejs")),
            PathBuf::from(r"C:\Program Files\nodejs")
        );
    }

    #[test]
    fn recognises_javascript_entry_points() {
        assert!(is_js_entry("/path/lib/bin.js"));
        assert!(is_js_entry("/path/lib/bin.mjs"));
        assert!(is_js_entry("/path/lib/bin.cjs"));
        assert!(!is_js_entry("/usr/local/bin/dsh"));
    }
}
