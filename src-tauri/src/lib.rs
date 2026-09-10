//! DeepSeek Harness Desktop — a native shell (Tauri + Rust) around the
//! `dsh web` browser GUI.
//!
//! The shell starts the real harness server, discovers its loopback URL, and
//! hosts it in a platform webview. No harness logic is reimplemented here.

mod backend;
mod logging;
#[cfg(feature = "portable")]
mod portable;
mod usage;

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
};

use tauri::{
    menu::{Menu, MenuItem, Submenu},
    webview::{NewWindowResponse, WebviewBuilder},
    window::WindowBuilder,
    AppHandle, LogicalPosition, LogicalSize, Manager, RunEvent, Webview, WebviewUrl, Window,
    WindowEvent,
};

/// How wide the status panel is, in logical pixels.
const PANEL_WIDTH: f64 = 320.0;

/// Height of the custom title bar. Only Linux uses a custom bar: macOS keeps
/// its native traffic lights and Windows keeps its snap affordances, so on
/// those platforms the bar is simply not created and the height is zero.
#[cfg(target_os = "linux")]
const TITLEBAR_HEIGHT: f64 = 38.0;
#[cfg(not(target_os = "linux"))]
const TITLEBAR_HEIGHT: f64 = 0.0;

/// The label of the custom title bar webview. Its commands refuse any other
/// caller, the same way the status panel does.
pub const TITLEBAR_LABEL: &str = "titlebar";

/// The application menu, kept as managed state so the title bar's menu button
/// can pop it up as a context menu on Linux where the native menu bar is
/// hidden with the decorations.
struct MenuState(Menu<tauri::Wry>);

/// How the status panel is displayed.
///
/// Three states because they answer different needs: `Pinned` gives the panel
/// its own column and shrinks the harness to fit, `Floating` overlays it for a
/// glance without disturbing the layout, and `Hidden` gets it out of the way.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum PanelMode {
    #[default]
    Pinned,
    Floating,
    Hidden,
}

impl PanelMode {
    fn from_id(id: &str) -> Option<Self> {
        match id {
            "panel-pin" => Some(Self::Pinned),
            "panel-float" => Some(Self::Floating),
            "panel-hide" => Some(Self::Hidden),
            _ => None,
        }
    }
}

/// The current panel mode, so a window resize can re-apply the same layout.
struct PanelState(Mutex<PanelMode>);

/// Lay the webviews out for the given mode.
///
/// Called on every resize as well as on every mode change. On Linux the child
/// webviews are reparented into a `gtk::Fixed` (Tauri's default GTK box stacks
/// children vertically and ignores bounds), so the geometry is applied to the
/// GTK widgets on the main thread instead of through Tauri's bounds API.
#[cfg(not(target_os = "linux"))]
fn layout(window: &Window, mode: PanelMode) {
    let Ok(size) = window.inner_size() else {
        return;
    };
    let scale = window.scale_factor().unwrap_or(1.0);
    let width = size.width as f64 / scale;
    let height = size.height as f64 / scale;

    let top = TITLEBAR_HEIGHT;
    let content_height = (height - top).max(1.0);

    if let Some(titlebar) = window.get_webview(TITLEBAR_LABEL) {
        let _ = titlebar.set_position(LogicalPosition::new(0.0, 0.0));
        let _ = titlebar.set_size(LogicalSize::new(width, top));
    }

    // The splash fills the content area only until the harness exists.
    if let Some(splash) = window.get_webview("splash") {
        if window.get_webview("harness").is_none() {
            let _ = splash.set_position(LogicalPosition::new(0.0, top));
            let _ = splash.set_size(LogicalSize::new(width, content_height));
        } else {
            let _ = splash.hide();
        }
    }

    // Never let the panel crowd out the harness on a narrow window.
    let panel_width = PANEL_WIDTH.min(width * 0.45);
    let harness_width = match mode {
        PanelMode::Pinned => (width - panel_width).max(1.0),
        _ => width,
    };

    if let Some(harness) = window.get_webview("harness") {
        let _ = harness.set_position(LogicalPosition::new(0.0, top));
        let _ = harness.set_size(LogicalSize::new(harness_width, content_height));
    }

    if let Some(panel) = window.get_webview(usage::PANEL_LABEL) {
        match mode {
            PanelMode::Hidden => {
                let _ = panel.hide();
            }
            _ => {
                let _ = panel.set_position(LogicalPosition::new(width - panel_width, top));
                let _ = panel.set_size(LogicalSize::new(panel_width, content_height));
                let _ = panel.show();
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod linux_layout {
    use super::*;
    use gtk::prelude::*;
    use std::{cell::RefCell, collections::HashMap};

    /// The fixed container plus every child webview, by label. Accessed only on
    /// the GTK main thread, which is why a thread-local cell is enough.
    struct State {
        fixed: Option<gtk::Fixed>,
        webviews: HashMap<&'static str, webkit2gtk::WebView>,
    }

    thread_local! {
        static STATE: RefCell<State> = RefCell::new(State {
            fixed: None,
            webviews: HashMap::new(),
        });
    }

    /// Move a webview from Tauri's vertical GTK box into our fixed container.
    pub fn attach(
        window: &Window,
        label: &'static str,
        webview: Webview,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) {
        let window = window.clone();
        let _ = webview.with_webview(move |platform| {
            let widget = platform.inner();
            let vbox = window.default_vbox().expect("GTK default vbox");

            STATE.with(|cell| {
                let mut state = cell.borrow_mut();
                if state.fixed.is_none() {
                    let fixed = gtk::Fixed::new();
                    vbox.pack_start(&fixed, true, true, 0);
                    fixed.show();
                    state.fixed = Some(fixed);
                }

                let fixed = state.fixed.clone().expect("fixed container");
                vbox.remove(&widget);
                fixed.put(&widget, x as i32, y as i32);
                widget.set_size_request(width.max(1.0) as i32, height.max(1.0) as i32);
                widget.size_allocate(&gtk::Allocation::new(
                    x as i32,
                    y as i32,
                    width.max(1.0) as i32,
                    height.max(1.0) as i32,
                ));
                widget.show();
                state.webviews.insert(label, widget);
            });
        });
    }

    /// The entry point shared with `lib::layout`: compute geometry, then apply
    /// it to the GTK widgets on the main thread.
    pub fn apply(window: &Window, mode: PanelMode) {
        let Ok(size) = window.inner_size() else {
            return;
        };
        let scale = window.scale_factor().unwrap_or(1.0);
        let width = size.width as f64 / scale;
        let height = size.height as f64 / scale;

        let top = TITLEBAR_HEIGHT;
        let content_height = (height - top).max(1.0);
        let panel_width = PANEL_WIDTH.min(width * 0.45);
        let harness_width = match mode {
            PanelMode::Pinned => (width - panel_width).max(1.0),
            _ => width,
        };

        let window = window.clone();
        let _ = window.run_on_main_thread(move || {
            STATE.with(|cell| {
                let state = cell.borrow();
                let Some(fixed) = state.fixed.as_ref() else {
                    return;
                };

                let place = |widget: &webkit2gtk::WebView, x: f64, y: f64, w: f64, h: f64| {
                    widget.set_size_request(w.max(1.0) as i32, h.max(1.0) as i32);
                    widget.size_allocate(&gtk::Allocation::new(
                        x as i32,
                        y as i32,
                        w.max(1.0) as i32,
                        h.max(1.0) as i32,
                    ));
                    fixed.move_(widget, x as i32, y as i32);
                };

                if let Some(widget) = state.webviews.get(TITLEBAR_LABEL) {
                    place(widget, 0.0, 0.0, width, top);
                }

                let has_harness = state.webviews.contains_key("harness");
                if let Some(widget) = state.webviews.get("splash") {
                    if has_harness {
                        widget.set_visible(false);
                    } else {
                        place(widget, 0.0, top, width, content_height);
                    }
                }

                if let Some(widget) = state.webviews.get("harness") {
                    place(widget, 0.0, top, harness_width, content_height);
                }
                if let Some(widget) = state.webviews.get(usage::PANEL_LABEL) {
                    match mode {
                        PanelMode::Hidden => widget.set_visible(false),
                        _ => {
                            place(
                                widget,
                                width - panel_width,
                                top,
                                panel_width,
                                content_height,
                            );
                            widget.set_visible(true);
                        }
                    }
                }
            });
        });
    }
}

#[cfg(target_os = "linux")]
fn layout(window: &Window, mode: PanelMode) {
    linux_layout::apply(window, mode);
}

use backend::Backend;

/// Managed state holding the live server process for shutdown.
///
/// `exiting` closes the race with the startup thread: the backend is spawned
/// off-thread and is only registered here once it announces its URL. If the
/// user closes the splash window before that, `RunEvent::Exit` must still be
/// able to stop the pending child, and a late `set` must not create a window
/// for an app that is already going away.
struct BackendState {
    backend: Mutex<Option<Backend>>,
    exiting: AtomicBool,
}

impl BackendState {
    /// Store the live backend. Returns `false` when the app is already exiting;
    /// in that case the backend has been shut down by this call and the caller
    /// must not continue creating the main window.
    fn set(&self, mut backend: Backend) -> bool {
        if self.exiting.load(Ordering::SeqCst) {
            backend.shutdown();
            return false;
        }
        if let Ok(mut guard) = self.backend.lock() {
            *guard = Some(backend);
            // On unix the pgid stays published for the signal handler; the
            // Windows pending-pid fallback is no longer needed once managed.
            backend::clear_pending();
            return true;
        }
        backend.shutdown();
        false
    }

    fn shutdown(&self) {
        self.exiting.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.backend.lock() {
            if let Some(mut backend) = guard.take() {
                backend.shutdown();
            }
        }
        // Covers a child that was spawned but never made it into `backend`.
        backend::kill_pending();
    }
}

/// Open a path or URL with whatever the desktop uses for it.
///
/// One implementation serves both the log-file menu items and the link
/// interceptor: `xdg-open`/`open`/`ShellExecuteW` all accept paths and URLs
/// alike.
///
/// Windows deliberately does **not** go through `cmd /C start`. Rust quotes
/// `Command` arguments by `CommandLineToArgvW` rules, and the escaping added
/// for CVE-2024-24576 only applies when the *program* is a `.bat`/`.cmd` — with
/// `cmd.exe` as the program, `&`, `|`, `^` and `%VAR%` in the argument are
/// re-parsed by the shell. The argument here is a link rendered by the harness
/// page, so page content would decide whether a second command runs.
fn open_with_system(target: &str) {
    #[cfg(windows)]
    windows_open::open(target);

    #[cfg(not(windows))]
    {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        // Reported rather than ignored: "Open Log File" silently doing nothing
        // is worse than one line in the log.
        if let Err(error) = std::process::Command::new(opener).arg(target).spawn() {
            crate::logging::line(&format!("dsh-desktop: could not open {target}: {error}"));
        }
    }
}

/// `ShellExecuteW` with the `open` verb, declared directly so one call does not
/// justify a Windows-API crate.
#[cfg(windows)]
mod windows_open {
    use std::ffi::{c_void, OsStr};
    use std::iter;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> *mut c_void;
    }

    /// `SW_SHOWNORMAL`: restore and activate, which is what a hand-off means.
    const SW_SHOWNORMAL: i32 = 1;

    /// A NUL-terminated UTF-16 copy of `text`.
    fn wide(text: &str) -> Vec<u16> {
        OsStr::new(text).encode_wide().chain(iter::once(0)).collect()
    }

    /// Hand `target` to the shell. Success is the documented contract that the
    /// returned value exceeds 32; there is no last-error to consult.
    pub fn open(target: &str) {
        let operation = wide("open");
        let file = wide(target);
        let result = unsafe {
            ShellExecuteW(
                ptr::null_mut(),
                operation.as_ptr(),
                file.as_ptr(),
                ptr::null(),
                ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if (result as isize) <= 32 {
            crate::logging::line(&format!(
                "dsh-desktop: ShellExecuteW could not open {target} (code {})",
                result as isize
            ));
        }
    }
}

/// Open a path with whatever the desktop uses for it.
fn open_externally(path: &std::path::Path) {
    let target = path.to_string_lossy();
    open_with_system(&target);
}

/// Is this URL the origin the harness is served from?
///
/// Navigation within the app is allowed only for the loopback origin we
/// started; everything else is sent to the system browser instead.
fn same_origin(url: &url::Url, origin: &url::Url) -> bool {
    url.scheme() == origin.scheme()
        && url.port_or_known_default() == origin.port_or_known_default()
        && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"))
}

/// Should a navigation that is not same-origin be handed to the desktop?
fn is_external_scheme(url: &url::Url) -> bool {
    matches!(url.scheme(), "http" | "https" | "mailto" | "tel")
}

/// Decide what one navigation does: stay in the webview, or open externally.
///
/// Loopback navigation to the backend origin stays. `http`/`https`/`mailto`/
/// `tel` destinations go to the system browser. Everything else (`data:`,
/// `blob:`, `about:`) is left to the webview so no plugin behaviour changes.
fn navigation_policy(url: &url::Url, origin: &url::Url) -> bool {
    if same_origin(url, origin) {
        return true;
    }
    if is_external_scheme(url) {
        open_with_system(url.as_str());
        return false;
    }
    true
}

/// A menu offering the two things needed to diagnose a problem: the log, and
/// the inspector.
///
/// Neither should require relaunching the app from a terminal. Asking a user
/// to do that is both awkward and misleading — it changes the conditions of
/// the run, and it hides the information until someone knows the trick.
fn build_menu(app: &tauri::App) -> tauri::Result<()> {
    let logs = MenuItem::with_id(app, "open-log", "Open Log File", true, None::<&str>)?;
    let folder = MenuItem::with_id(app, "open-log-dir", "Show Log Folder", true, None::<&str>)?;
    let inspector = MenuItem::with_id(
        app,
        "devtools",
        "Developer Tools",
        true,
        Some("CmdOrCtrl+Shift+I"),
    )?;
    let reload = MenuItem::with_id(app, "reload", "Reload", true, Some("CmdOrCtrl+R"))?;

    let pin = MenuItem::with_id(
        app,
        "panel-pin",
        "Pin Status Panel",
        true,
        Some("CmdOrCtrl+1"),
    )?;
    let float = MenuItem::with_id(
        app,
        "panel-float",
        "Float Status Panel",
        true,
        Some("CmdOrCtrl+2"),
    )?;
    let hide = MenuItem::with_id(
        app,
        "panel-hide",
        "Hide Status Panel",
        true,
        Some("CmdOrCtrl+0"),
    )?;
    let view = Submenu::with_items(app, "View", true, &[&pin, &float, &hide])?;

    let submenu = Submenu::with_items(
        app,
        "Troubleshooting",
        true,
        &[&reload, &inspector, &logs, &folder],
    )?;

    // Linux starts from an empty menu: the native bar is hidden, so the only
    // entries should be the two this app actually needs. macOS and Windows
    // keep the platform's default Edit/Window/Help submenus.
    #[cfg(target_os = "linux")]
    let menu = Menu::new(app.handle())?;
    #[cfg(not(target_os = "linux"))]
    let menu = Menu::default(app.handle())?;
    menu.append(&view)?;
    menu.append(&submenu)?;
    // The Linux custom title bar needs the same menu to show at the cursor
    // when its menu button is pressed. It is never attached to the Linux
    // window: attaching it would pack a visible GtkMenuBar into the client
    // area even with decorations disabled.
    #[cfg(target_os = "linux")]
    app.manage(MenuState(menu));
    #[cfg(not(target_os = "linux"))]
    {
        app.manage(MenuState(menu.clone()));
        app.set_menu(menu)?;
    }

    app.on_menu_event(move |app, event| match event.id().as_ref() {
        id if PanelMode::from_id(id).is_some() => {
            let mode = PanelMode::from_id(id).expect("checked by the guard");
            if let Some(state) = app.try_state::<PanelState>() {
                if let Ok(mut current) = state.0.lock() {
                    *current = mode;
                }
            }
            if let Some(window) = app.get_window("main") {
                layout(&window, mode);
            }
        }
        "open-log" => {
            if let Some(path) = logging::path() {
                open_externally(path);
            }
        }
        "open-log-dir" => {
            if let Some(dir) = logging::path().and_then(|p| p.parent()) {
                open_externally(dir);
            }
        }
        "devtools" => {
            if let Some(window) = app.get_window("main") {
                if let Some(harness) = window.get_webview("harness") {
                    harness.open_devtools();
                }
            }
        }
        "reload" => {
            if let Some(window) = app.get_window("main") {
                if let Some(harness) = window.get_webview("harness") {
                    let _ = harness.eval("window.location.reload()");
                }
            }
        }
        _ => {}
    });

    Ok(())
}

/// Create the one and only application window, at startup.
///
/// The content area initially hosts the splash page. When the backend
/// announces its URL, [`activate_harness`] swaps that webview for the harness
/// and the status panel in the same window, so the startup page appears to
/// refresh into the app instead of being replaced by a second window.
fn create_shell_window(app: &tauri::App) -> tauri::Result<Window> {
    let builder = WindowBuilder::new(app, "main")
        .title("DeepSeek Harness Desktop")
        .inner_size(1280.0, 800.0)
        .min_inner_size(900.0, 600.0)
        .resizable(true)
        .center();

    // Linux gets a custom title bar, so the native one has to go. macOS and
    // Windows keep their native decorations.
    #[cfg(target_os = "linux")]
    let builder = builder.decorations(false);

    let window = builder.build()?;

    #[cfg(target_os = "linux")]
    {
        let titlebar = window.add_child(
            WebviewBuilder::new(TITLEBAR_LABEL, WebviewUrl::App("titlebar.html".into())),
            LogicalPosition::new(0.0, 0.0),
            LogicalSize::new(1280.0, TITLEBAR_HEIGHT),
        )?;
        linux_layout::attach(
            &window,
            TITLEBAR_LABEL,
            titlebar,
            0.0,
            0.0,
            1280.0,
            TITLEBAR_HEIGHT,
        );
    }

    let splash = window.add_child(
        WebviewBuilder::new("splash", WebviewUrl::App("index.html".into())),
        LogicalPosition::new(0.0, TITLEBAR_HEIGHT),
        LogicalSize::new(1280.0, 800.0 - TITLEBAR_HEIGHT),
    )?;
    #[cfg(target_os = "linux")]
    linux_layout::attach(
        &window,
        "splash",
        splash,
        0.0,
        TITLEBAR_HEIGHT,
        1280.0,
        800.0 - TITLEBAR_HEIGHT,
    );

    layout(&window, PanelMode::default());
    Ok(window)
}

/// An initialization script that restores the harness's last current session.
///
/// The backend listens on a fresh loopback port every launch, so the webview
/// sees a new origin and the harness's `localStorage`-persisted selection
/// would otherwise be empty — which is why a new session appeared on every
/// start. Seed the persisted `dsh.sessions.current` key before the page's
/// scripts run, once per webview lifetime; `sessionStorage` keeps a reload
/// from overwriting a selection the user makes later in the same run.
fn resume_session_script() -> String {
    let Some(id) = usage::latest_session_id() else {
        return String::new();
    };
    let id = serde_json::to_string(&id).unwrap_or_else(|_| "null".to_string());
    format!(
        "(()=>{{try{{if(!sessionStorage.getItem('dsh-desktop-seeded')){{\
         localStorage.setItem('dsh.sessions.current',JSON.stringify({{sessionId:{id}}}));\
         sessionStorage.setItem('dsh-desktop-seeded','1')\
         }}}}catch(_){{}}}})();"
    )
}

/// Swap the startup splash for the live harness and status panel.
///
/// Separate webviews rather than one, because the panel and title bar must not
/// be injected into the harness's page. Injecting would mean our markup living
/// inside a UI we do not control, breaking whenever it changes — and this
/// shell's whole point is that it leaves the harness alone.
fn activate_harness(app: &AppHandle, url: &str, mode: PanelMode) -> tauri::Result<()> {
    let window = app.get_window("main").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "the main window is gone")
    })?;
    let target = url
        .parse::<url::Url>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let origin = target.clone();

    // The splash stays in the window, hidden under the new webviews. Keeping it
    // avoids destroying a webview mid-session, and on Linux the fixed container
    // already layers later children above it.
    if let Some(splash) = window.get_webview("splash") {
        let _ = splash.hide();
    }

    // Each closure below needs its own copy of the origin because both are
    // `'static` `Fn`s kept by the webview.
    let nav_origin = origin.clone();
    let new_window_origin = origin.clone();
    let resume_script = resume_session_script();
    let harness = WebviewBuilder::new("harness", WebviewUrl::External(target))
        .initialization_script(resume_script.as_str())
        .on_navigation(move |url| navigation_policy(url, &nav_origin))
        .on_new_window(move |url, _features| {
            if navigation_policy(&url, &new_window_origin) {
                NewWindowResponse::Allow
            } else {
                NewWindowResponse::Deny
            }
        });

    let harness_webview = window.add_child(
        harness,
        LogicalPosition::new(0.0, TITLEBAR_HEIGHT),
        LogicalSize::new(1280.0, 800.0 - TITLEBAR_HEIGHT),
    )?;
    #[cfg(target_os = "linux")]
    linux_layout::attach(
        &window,
        "harness",
        harness_webview,
        0.0,
        TITLEBAR_HEIGHT,
        1280.0,
        800.0 - TITLEBAR_HEIGHT,
    );

    let panel = window.add_child(
        WebviewBuilder::new(usage::PANEL_LABEL, WebviewUrl::App("panel.html".into())),
        LogicalPosition::new(960.0, TITLEBAR_HEIGHT),
        LogicalSize::new(PANEL_WIDTH, 800.0 - TITLEBAR_HEIGHT),
    )?;
    #[cfg(target_os = "linux")]
    linux_layout::attach(
        &window,
        usage::PANEL_LABEL,
        panel,
        960.0,
        TITLEBAR_HEIGHT,
        PANEL_WIDTH,
        800.0 - TITLEBAR_HEIGHT,
    );

    layout(&window, mode);
    Ok(())
}

/// The custom title bar is the only webview allowed to drive window chrome.
fn require_titlebar(webview: &Webview) -> Result<Window, String> {
    if webview.label() != TITLEBAR_LABEL {
        return Err("window controls are only available to the title bar".to_string());
    }
    Ok(webview.window())
}

/// Pop up the regular View / Troubleshooting menu under the clicked button.
///
/// The cursor-position variant is unreliable under Wayland, so the title bar
/// passes its click coordinates and the menu is anchored explicitly relative
/// to the window.
#[tauri::command]
fn titlebar_menu(webview: Webview, x: f64, y: f64) -> Result<(), String> {
    let window = require_titlebar(&webview)?;
    let menu = window.app_handle().state::<MenuState>().0.clone();
    window
        .popup_menu_at(&menu, LogicalPosition::new(x, y + TITLEBAR_HEIGHT))
        .map_err(|e| e.to_string())
}

/// Minimize, maximize/restore, or close from the custom title bar.
#[tauri::command]
fn titlebar_window(webview: Webview, action: String) -> Result<(), String> {
    let window = require_titlebar(&webview)?;
    match action.as_str() {
        "minimize" => window.minimize(),
        "maximize" => {
            if window.is_maximized().map_err(|e| e.to_string())? {
                window.unmaximize()
            } else {
                window.maximize()
            }
        }
        "close" => window.close(),
        _ => return Err(format!("unknown title bar action: {action}")),
    }
    .map_err(|e| e.to_string())
}

/// The splash webview inside the main window, if it is still visible.
fn splash_webview(app: &AppHandle) -> Option<Webview> {
    app.get_window("main")?.get_webview("splash")
}

/// Stream a line of backend output into the splash page.
fn report_progress(app: &AppHandle, line: &str) {
    let Some(splash) = splash_webview(app) else {
        return;
    };
    if let Ok(payload) = serde_json::to_string(line) {
        let _ = splash.eval(format!(
            "window.dshProgress && window.dshProgress({payload})"
        ));
    }
}

/// Render a startup failure inside the splash page.
///
/// A GUI app has no console, so `eprintln!` alone means the user sees the
/// window vanish with no explanation. The message is passed through
/// `serde_json` so quotes and newlines in a backend error cannot break out of
/// the JavaScript string.
fn report_failure(app: &AppHandle, message: &str) {
    logging::line(&format!("dsh-desktop: {message}"));
    let payload = serde_json::to_string(message)
        .unwrap_or_else(|_| "\"the backend failed to start\"".to_string());
    let Some(splash) = splash_webview(app) else {
        return;
    };
    let _ = splash.eval(format!(
        "window.dshShowError && window.dshShowError({payload})"
    ));
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            app.manage(BackendState {
                backend: Mutex::new(None),
                exiting: AtomicBool::new(false),
            });
            app.manage(PanelState(Mutex::new(PanelMode::default())));
            backend::install_signal_handlers();

            let log = app
                .path()
                .app_log_dir()
                .unwrap_or_else(|_| std::env::temp_dir())
                .join("dsh-desktop.log");
            logging::init(log);
            logging::line(&format!(
                "dsh-desktop {} starting",
                env!("CARGO_PKG_VERSION")
            ));

            if let Err(e) = build_menu(app) {
                logging::line(&format!("dsh-desktop: could not build the menu: {e}"));
            }

            let roots = backend::search_roots(app);
            let shell = create_shell_window(app)?;
            let resized = shell.clone();
            shell.on_window_event(move |event| {
                if let WindowEvent::Resized(_) = event {
                    let mode = resized
                        .try_state::<PanelState>()
                        .and_then(|s| s.0.lock().ok().map(|m| *m))
                        .unwrap_or_default();
                    layout(&resized, mode);
                }
            });
            let handle = app.handle().clone();

            // Starting the backend must not block `setup`: nothing renders
            // until the setup hook returns, so waiting here would hide the
            // window we just created.
            thread::spawn(move || {
                let progress = handle.clone();
                match backend::spawn_backend(&roots, move |line| report_progress(&progress, line)) {
                    Ok((backend, url)) => {
                        // A late success after the window was closed means the
                        // app is already tearing down; `set` shut the backend
                        // down and we must not open the harness.
                        if !handle.state::<BackendState>().set(backend) {
                            return;
                        }
                        let mode = handle
                            .try_state::<PanelState>()
                            .and_then(|s| s.0.lock().ok().map(|m| *m))
                            .unwrap_or_default();
                        if let Err(e) = activate_harness(&handle, &url, mode) {
                            report_failure(&handle, &format!("could not open the window: {e}"));
                            handle.state::<BackendState>().shutdown();
                        }
                    }
                    // The splash page stays open holding the message, so the
                    // user can read it and close the app themselves.
                    Err(message) => report_failure(&handle, &message),
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            usage::usage_snapshot,
            usage::refresh_balance,
            titlebar_menu,
            titlebar_window
        ])
        .build(tauri::generate_context!())
        .expect("error while building the DeepSeek Harness Desktop app");

    app.run(|app_handle, event| {
        if let RunEvent::Exit = event {
            if let Some(state) = app_handle.try_state::<BackendState>() {
                state.shutdown();
            }
        }
    });
}
