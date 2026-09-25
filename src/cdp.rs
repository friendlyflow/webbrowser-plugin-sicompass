//! Driving Chrome over the DevTools protocol (CDP), blocking.
//!
//! Chrome is started with `--remote-debugging-pipe`: it reads protocol
//! messages on file descriptor 3 and writes them on 4, each one JSON ended by
//! a NUL byte. No network port is opened, so nothing else on the machine can
//! drive it. In the sandbox the host starts it with
//! `process.child.spawn-with-channel`, which puts those two descriptors on a
//! message channel. Natively (the live tests) the pipes are made here.
//!
//! Only the handful of calls the browser makes are here: a tab
//! (`Target.createTarget`, attached with a flat session), navigation, script
//! evaluation, the viewport, cookies and closing. All of it runs in the
//! browser task, one call at a time, so every wait is a poll of the pipe with
//! a deadline.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The viewport every page is rendered at.
///
/// Responsive sites pick their layout from this, so it decides whether the
/// reader gets the desktop page or the phone one. Wide enough to clear the
/// usual `xl` breakpoint (1200px) with room to spare, which is checked at
/// compile time below. Set on every tab by [`Chrome::set_desktop_viewport`],
/// belt and braces next to `--window-size`: headless defaults to 800x600, and
/// under Xvfb the window is clamped to the virtual screen.
pub const VIEWPORT_W: u32 = 1920;
const _: () = assert!(VIEWPORT_W >= 1200, "must clear the usual xl breakpoint");
pub const VIEWPORT_H: u32 = 1080;

/// How long Chrome gets to answer its first call after starting.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);

/// The names Chrome goes by, in the order they are tried. Each one is a
/// program the plugin's `process` grant lists: on Linux the commands on
/// `PATH`, on macOS the applications (found in `/Applications`).
pub const CHROME_NAMES: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "google-chrome-beta",
    "chromium",
    "chromium-browser",
    "chrome",
    "Google Chrome",
    "Chromium",
    "Microsoft Edge",
    "Google Chrome Canary",
];

/// The virtual X server a headed Chrome is put on, when there is one.
pub const XVFB: &str = "Xvfb";

/// An address that is not an accessibility bus: Chrome registers with the
/// session's bus when an assistive technology runs, and an off-screen browser
/// must never show up to the user's screen reader.
const NO_AT_SPI_BUS: &str = "unix:path=/nonexistent/sicompass-keeps-chrome-off-the-a11y-bus";

/// Chrome's flags, as chromiumoxide (which the browser used before) set them,
/// plus the browser's own.
fn chrome_args(headless: bool, user_data_dir: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "--disable-background-networking",
        "--enable-features=NetworkService,NetworkServiceInProcess",
        "--disable-background-timer-throttling",
        "--disable-backgrounding-occluded-windows",
        "--disable-breakpad",
        "--disable-client-side-phishing-detection",
        "--disable-component-extensions-with-background-pages",
        "--disable-default-apps",
        "--disable-dev-shm-usage",
        "--disable-features=TranslateUI",
        "--disable-hang-monitor",
        "--disable-ipc-flooding-protection",
        "--disable-popup-blocking",
        "--disable-prompt-on-repost",
        "--disable-renderer-backgrounding",
        "--disable-sync",
        "--force-color-profile=srgb",
        "--metrics-recording-only",
        "--no-first-run",
        "--enable-automation",
        "--password-store=basic",
        "--use-mock-keychain",
        "--enable-blink-features=IdleDetection",
        "--lang=en_US",
        "--disable-extensions",
        "--disable-blink-features=AutomationControlled",
        "--no-default-browser-check",
        "--remote-debugging-pipe",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    args.push(format!("--user-data-dir={user_data_dir}"));
    args.push(format!("--window-size={VIEWPORT_W},{VIEWPORT_H}"));
    if headless {
        // `--headless=new`, the same browser as headed Chrome, so the stealth
        // script still has a real window.chrome and a full DOM to patch.
        args.extend(["--headless=new", "--hide-scrollbars", "--mute-audio"].map(str::to_owned));
    } else {
        args.push("--ozone-platform=x11".to_owned());
    }
    args.push("about:blank".to_owned());
    args
}

/// Chrome's environment: never the session's accessibility bus, and, on a
/// virtual display, that display rather than the compositor. Returns what to
/// set and what to remove.
fn chrome_env(display: Option<u32>) -> (Vec<(String, String)>, Vec<String>) {
    let mut env = vec![("AT_SPI_BUS_ADDRESS".to_owned(), NO_AT_SPI_BUS.to_owned())];
    let mut unset = Vec::new();
    if let Some(d) = display {
        env.push(("DISPLAY".to_owned(), format!(":{d}")));
        // Chrome must not pick the compositor over the virtual X11 display.
        unset.push("WAYLAND_DISPLAY".to_owned());
    }
    (env, unset)
}

/// Xvfb's arguments: a display of its own choosing, written to its stdout,
/// and a desktop-sized screen. Left to a default, the screen was 612x459,
/// Chrome clamped its window to it, and every responsive site served its
/// phone layout. `-terminate`: it exits when its last client (Chrome) goes,
/// so it never outlives Chrome, even when the plugin is gone before it could
/// stop it (the app killed): Chrome exits when its pipe closes.
fn xvfb_args() -> Vec<String> {
    [
        "-displayfd".to_owned(),
        "1".to_owned(),
        "-screen".to_owned(),
        "0".to_owned(),
        format!("{VIEWPORT_W}x{VIEWPORT_H}x24"),
        "-nolisten".to_owned(),
        "tcp".to_owned(),
        "-terminate".to_owned(),
    ]
    .into()
}

// ---------------------------------------------------------------------------
// The pipe
// ---------------------------------------------------------------------------

/// A started program with a message channel: Chrome, or Xvfb (no channel).
#[cfg(target_arch = "wasm32")]
pub struct Proc {
    child: sicompass_pdk::process::Child,
}

#[cfg(target_arch = "wasm32")]
impl Proc {
    fn start(
        program: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        unset: &[String],
        channel: bool,
    ) -> Result<Proc, String> {
        use sicompass_pdk::process::Child;
        let child = if channel {
            Child::spawn_with_channel(program, args, cwd, env, unset)?
        } else {
            Child::spawn(program, args, cwd, env, unset, None)?
        };
        Ok(Proc { child })
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.child.channel_write(bytes)
    }

    fn receive(&mut self) -> Vec<u8> {
        self.child.channel_read(1 << 20)
    }

    fn stdout(&mut self) -> Vec<u8> {
        self.child.read(4096)
    }

    fn stderr(&mut self) -> Vec<u8> {
        self.child.read_stderr(1 << 16)
    }

    fn exited(&mut self) -> bool {
        self.child.try_wait().is_some()
    }

    fn kill(&mut self) {
        self.child.kill();
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub struct Proc {
    child: std::process::Child,
    to_child: Option<std::fs::File>,
    from_child: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    stdout: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    stderr: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Proc {
    /// Natively, `program` is looked up on `PATH` as the host would, and the
    /// channel is a pair of pipes on file descriptors 3 and 4.
    fn start(
        program: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        unset: &[String],
        channel: bool,
    ) -> Result<Proc, String> {
        use std::io::Read;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        fn reader(mut r: impl Read + Send + 'static) -> std::sync::mpsc::Receiver<Vec<u8>> {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 1 << 16];
                while let Ok(n) = r.read(&mut buf) {
                    if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
            });
            rx
        }
        fn pipe() -> Result<(std::fs::File, std::fs::File), String> {
            let mut fds = [0i32; 2];
            // SAFETY: `fds` has room for the two descriptors pipe(2) writes.
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            // SAFETY: both were just opened, and nothing else owns them.
            Ok(unsafe {
                (
                    std::fs::File::from_raw_fd(fds[0]),
                    std::fs::File::from_raw_fd(fds[1]),
                )
            })
        }

        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        for k in unset {
            cmd.env_remove(k);
        }
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
        let mut ends = None;
        if channel {
            let (child_reads, to_child) = pipe()?;
            let (from_child, child_writes) = pipe()?;
            let (r, w) = (child_reads.as_raw_fd(), child_writes.as_raw_fd());
            // SAFETY: only dup2 between fork and exec, which is
            // async-signal-safe. dup2 clears close-on-exec on 3 and 4.
            unsafe {
                cmd.pre_exec(move || {
                    if libc::dup2(r, 3) < 0 || libc::dup2(w, 4) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            ends = Some((child_reads, to_child, from_child, child_writes));
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start `{program}`: {e}"))?;
        let (to_child, from_child) = match ends {
            Some((child_reads, to_child, from_child, child_writes)) => {
                drop(child_reads);
                drop(child_writes);
                (Some(to_child), Some(reader(from_child)))
            }
            None => (None, None),
        };
        let stdout = child.stdout.take().map(reader);
        let stderr = child.stderr.take().map(reader);
        Ok(Proc {
            child,
            to_child,
            from_child,
            stdout,
            stderr,
        })
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        use std::io::Write;
        match &mut self.to_child {
            Some(f) => f.write_all(bytes).map_err(|e| e.to_string()),
            None => Err("no channel".to_owned()),
        }
    }

    fn drain(rx: &Option<std::sync::mpsc::Receiver<Vec<u8>>>) -> Vec<u8> {
        rx.as_ref()
            .map(|rx| rx.try_iter().flatten().collect())
            .unwrap_or_default()
    }

    fn receive(&mut self) -> Vec<u8> {
        Self::drain(&self.from_child)
    }

    fn stdout(&mut self) -> Vec<u8> {
        Self::drain(&self.stdout)
    }

    fn stderr(&mut self) -> Vec<u8> {
        Self::drain(&self.stderr)
    }

    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Proc {
    fn drop(&mut self) {
        // Never leave a Chrome (or its Xvfb) behind: the old browser tests
        // leaked one each and took the desktop down with them.
        self.kill();
    }
}

/// Whether `program` can be started (a name the grant lists and the host
/// finds). Natively, whether it is on `PATH`.
fn available(program: &str) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        sicompass_pdk::process::which(program).is_ok()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(program).is_file()))
    }
}

/// The first Chrome that can be started.
pub fn find_chrome() -> Option<&'static str> {
    CHROME_NAMES.iter().copied().find(|n| available(n))
}

pub fn chrome_missing_message() -> String {
    "Chrome, Chromium or Edge was not found. Install one of them to use the web browser.".to_owned()
}

// ---------------------------------------------------------------------------
// Chrome
// ---------------------------------------------------------------------------

/// A tab: its target, and the session its commands go to.
#[derive(Debug, Clone)]
pub struct Page {
    target_id: String,
    session_id: String,
}

/// A running Chrome, and the virtual display it is on, if any.
pub struct Chrome {
    proc: Proc,
    xvfb: Option<Proc>,
    /// Bytes read that do not make a whole message yet.
    buf: Vec<u8>,
    next_id: u64,
    /// Events read while waiting for something else.
    events: VecDeque<Value>,
    /// Answers read while waiting for another one (a call that timed out
    /// earlier answers late).
    answers: VecDeque<Value>,
}

/// How a Chrome is launched.
pub struct Launch<'a> {
    /// The Chrome program name.
    pub chrome: &'a str,
    /// The working directory for Chrome, where its profile is
    /// (`--user-data-dir` is relative to it): a folder in the plugin's
    /// storage, which the host maps to the real one.
    pub profile_parent: &'a str,
    /// The profile folder's name in `profile_parent`.
    pub profile: &'a str,
}

impl Chrome {
    /// Start Chrome: headed on a virtual display when Xvfb is there, which is
    /// what sites that turn headless Chrome away accept, and headless
    /// otherwise.
    pub fn launch(opts: &Launch) -> Result<Chrome, String> {
        let xvfb = if available(XVFB) {
            match start_xvfb() {
                Ok(x) => Some(x),
                Err(e) => {
                    log(&format!(
                        "Xvfb did not start ({e}); running Chrome headless"
                    ));
                    None
                }
            }
        } else {
            None
        };
        let (env, unset) = chrome_env(xvfb.as_ref().map(|(_, d)| *d));
        let args = chrome_args(xvfb.is_none(), opts.profile);
        let proc = Proc::start(
            opts.chrome,
            &args,
            Some(opts.profile_parent),
            &env,
            &unset,
            true,
        )?;
        let mut chrome = Chrome {
            proc,
            xvfb: xvfb.map(|(p, _)| p),
            buf: Vec::new(),
            next_id: 0,
            events: VecDeque::new(),
            answers: VecDeque::new(),
        };
        // The first answer says the pipe works.
        chrome
            .call(None, "Browser.getVersion", json!({}), LAUNCH_TIMEOUT)
            .map_err(|e| {
                let stderr = String::from_utf8_lossy(&chrome.proc.stderr()).into_owned();
                let tail: String = stderr.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
                format!("Chrome ({}) did not start: {e} {tail}", opts.chrome)
            })?;
        Ok(chrome)
    }

    /// Open a tab on `about:blank`.
    pub fn new_page(&mut self) -> Result<Page, String> {
        let t = Duration::from_secs(15);
        let target = self.call(
            None,
            "Target.createTarget",
            json!({"url": "about:blank"}),
            t,
        )?;
        let target_id = target["targetId"]
            .as_str()
            .ok_or("Chrome opened no tab")?
            .to_owned();
        let attached = self.call(
            None,
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
            t,
        )?;
        let session_id = attached["sessionId"]
            .as_str()
            .ok_or("Chrome did not attach to the tab")?
            .to_owned();
        let page = Page {
            target_id,
            session_id,
        };
        self.page_call(&page, "Page.enable", json!({}), t)?;
        Ok(page)
    }

    /// Run `source` in every document the tab loads from now on.
    pub fn add_script_on_new_document(&mut self, page: &Page, source: &str) -> Result<(), String> {
        self.page_call(
            page,
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": source}),
            Duration::from_secs(10),
        )
        .map(drop)
    }

    /// A desktop layout, whatever the window or the virtual screen is.
    pub fn set_desktop_viewport(&mut self, page: &Page) {
        let _ = self.page_call(
            page,
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": VIEWPORT_W, "height": VIEWPORT_H,
                "deviceScaleFactor": 1.0, "mobile": false,
            }),
            Duration::from_secs(5),
        );
    }

    /// Navigate and wait for the load event, up to `timeout`.
    pub fn goto(&mut self, page: &Page, url: &str, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        // A load event from an earlier page must not end this wait.
        self.events.retain(|e| {
            !(e["method"] == "Page.loadEventFired" && e["sessionId"] == page.session_id.as_str())
        });
        let nav = self.page_call(page, "Page.navigate", json!({"url": url}), timeout)?;
        if let Some(err) = nav["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(err.to_owned());
        }
        self.wait_event(page, "Page.loadEventFired", deadline)
            .ok_or_else(|| {
                format!(
                    "navigation to {url} timed out after {} s",
                    timeout.as_secs()
                )
            })
            .map(drop)
    }

    /// Evaluate a JavaScript expression (awaiting a promise), and return its
    /// value.
    pub fn evaluate(&mut self, page: &Page, js: &str, timeout: Duration) -> Result<Value, String> {
        let r = self.page_call(
            page,
            "Runtime.evaluate",
            json!({"expression": js, "returnByValue": true, "awaitPromise": true}),
            timeout,
        )?;
        if let Some(ex) = r.get("exceptionDetails") {
            let text = ex["exception"]["description"]
                .as_str()
                .or_else(|| ex["text"].as_str())
                .unwrap_or("script error");
            return Err(text.to_owned());
        }
        Ok(r["result"]["value"].clone())
    }

    /// [`Chrome::evaluate`], read as `T`.
    pub fn evaluate_as<T: serde::de::DeserializeOwned>(
        &mut self,
        page: &Page,
        js: &str,
        timeout: Duration,
    ) -> Result<T, String> {
        let v = self.evaluate(page, js, timeout)?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    /// Where the tab is now.
    pub fn url(&mut self, page: &Page) -> Option<String> {
        self.evaluate_as::<String>(page, "window.location.href", Duration::from_secs(3))
            .ok()
    }

    /// The document as HTML, doctype included.
    pub fn content(&mut self, page: &Page, timeout: Duration) -> Result<String, String> {
        const JS: &str = "(() => { let r = ''; \
            if (document.doctype) r = new XMLSerializer().serializeToString(document.doctype); \
            if (document.documentElement) r += document.documentElement.outerHTML; \
            return r; })()";
        self.evaluate_as(page, JS, timeout)
    }

    /// Forget every cookie.
    pub fn clear_cookies(&mut self, page: &Page) -> Result<(), String> {
        self.page_call(
            page,
            "Network.clearBrowserCookies",
            json!({}),
            Duration::from_secs(5),
        )
        .map(drop)
    }

    /// Close a tab.
    pub fn close_page(&mut self, page: &Page) {
        let _ = self.call(
            None,
            "Target.closeTarget",
            json!({"targetId": page.target_id}),
            Duration::from_secs(3),
        );
    }

    /// Close Chrome, and the display it was on.
    pub fn close(mut self) {
        let _ = self.call(None, "Browser.close", json!({}), Duration::from_millis(500));
        self.proc.kill();
        if let Some(x) = &mut self.xvfb {
            x.kill();
        }
    }

    /// Whether Chrome is still running.
    pub fn alive(&mut self) -> bool {
        !self.proc.exited()
    }

    // ---- The protocol ----------------------------------------------------

    fn page_call(
        &mut self,
        page: &Page,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        self.call(Some(&page.session_id), method, params, timeout)
    }

    /// Send a command and wait for its answer.
    fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let mut bytes = serde_json::to_vec(&msg).map_err(|e| e.to_string())?;
        bytes.push(0);
        self.proc
            .send(&bytes)
            .map_err(|e| format!("{method}: {e}"))?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pos) = self.answers.iter().position(|a| a["id"] == id) {
                let answer = self.answers.remove(pos).expect("found above");
                if let Some(err) = answer.get("error") {
                    let text = err["message"].as_str().unwrap_or("error");
                    return Err(format!("{method}: {text}"));
                }
                return Ok(answer["result"].clone());
            }
            if Instant::now() >= deadline {
                return Err(format!("{method} timed out"));
            }
            if !self.pump() {
                if self.proc.exited() {
                    return Err(format!("{method}: Chrome has exited"));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    /// Wait for an event of `method` on `page`'s session.
    fn wait_event(&mut self, page: &Page, method: &str, deadline: Instant) -> Option<Value> {
        loop {
            if let Some(pos) = self
                .events
                .iter()
                .position(|e| e["method"] == method && e["sessionId"] == page.session_id.as_str())
            {
                return self.events.remove(pos);
            }
            if Instant::now() >= deadline || self.proc.exited() {
                return None;
            }
            if !self.pump() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// Read what Chrome sent, sorting answers from events. `false` when
    /// there was nothing.
    fn pump(&mut self) -> bool {
        let got = self.proc.receive();
        if got.is_empty() {
            return false;
        }
        self.buf.extend_from_slice(&got);
        while let Some(end) = self.buf.iter().position(|&b| b == 0) {
            let msg: Vec<u8> = self.buf.drain(..=end).collect();
            let Ok(v) = serde_json::from_slice::<Value>(&msg[..msg.len() - 1]) else {
                continue;
            };
            if v.get("id").is_some() {
                self.answers.push_back(v);
            } else {
                self.events.push_back(v);
                // Events nobody waits for (network, console) would pile up.
                if self.events.len() > 512 {
                    self.events.pop_front();
                }
            }
        }
        true
    }
}

/// Start Xvfb on a display of its choosing (`-displayfd 1`: it writes the
/// number it took to its stdout), and wait for that number.
fn start_xvfb() -> Result<(Proc, u32), String> {
    let mut proc = Proc::start(XVFB, &xvfb_args(), None, &[], &[], false)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut out = Vec::new();
    loop {
        out.extend(proc.stdout());
        if let Some(end) = out.iter().position(|&b| b == b'\n') {
            let number = String::from_utf8_lossy(&out[..end]).trim().parse::<u32>();
            return number.map(|n| (proc, n)).map_err(|e| e.to_string());
        }
        if proc.exited() {
            return Err(String::from_utf8_lossy(&proc.stderr()).trim().to_owned());
        }
        if Instant::now() >= deadline {
            proc.kill();
            return Err("no display number after 10 s".to_owned());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn log(msg: &str) {
    #[cfg(target_arch = "wasm32")]
    sicompass_pdk::host::log(msg);
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("webbrowser: {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_talks_over_its_pipe_with_no_port() {
        let args = chrome_args(true, "profile");
        assert!(args.contains(&"--remote-debugging-pipe".to_owned()));
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("--remote-debugging-port"))
        );
        assert!(args.contains(&"--user-data-dir=profile".to_owned()));
        assert!(args.contains(&"--headless=new".to_owned()));
        let headed = chrome_args(false, "profile");
        assert!(!headed.iter().any(|a| a.starts_with("--headless")));
        assert!(headed.contains(&"--ozone-platform=x11".to_owned()));
    }

    // Without Xvfb (a stock Mint / Ubuntu / Fedora desktop) Chrome has to run
    // headless. The old behaviour here was a visible Chrome window that stole
    // the keyboard focus, and with it the screen reader.
    #[test]
    fn no_display_means_headless_not_a_visible_window() {
        let (env, unset) = chrome_env(None);
        assert!(!env.iter().any(|(k, _)| k == "DISPLAY"));
        assert!(unset.is_empty());
        assert!(chrome_args(true, "p").contains(&"--headless=new".to_owned()));
    }

    // With a virtual display, Chrome runs headed on it, on X11.
    #[test]
    fn a_virtual_display_runs_chrome_headed_on_x11() {
        let (env, unset) = chrome_env(Some(7));
        assert!(env.contains(&("DISPLAY".to_owned(), ":7".to_owned())));
        assert_eq!(unset, ["WAYLAND_DISPLAY"]);
        let args = chrome_args(false, "p");
        assert!(!args.iter().any(|a| a.starts_with("--headless")));
        assert!(args.contains(&"--ozone-platform=x11".to_owned()));
    }

    // The screen and the window agree on a desktop size.
    #[test]
    fn the_virtual_screen_is_desktop_sized() {
        assert!(xvfb_args().contains(&format!("{VIEWPORT_W}x{VIEWPORT_H}x24")));
        assert!(
            chrome_args(false, "p").contains(&format!("--window-size={VIEWPORT_W},{VIEWPORT_H}"))
        );
        // Xvfb chooses a free display itself and says which on stdout.
        let args = xvfb_args();
        assert_eq!(&args[..2], ["-displayfd", "1"]);
        // And goes when Chrome goes, whoever stops Chrome.
        assert!(args.contains(&"-terminate".to_owned()));
    }

    /// Xvfb hides Chrome from the screen, not from the screen reader: the
    /// accessibility bus is per-session, so an off-screen Chrome would still
    /// register as a second "Google Chrome" application for Orca to wander
    /// into, and the user's arrow keys would stop reaching sicompass. The fix
    /// rests entirely on Chrome being unable to resolve the address below, so
    /// that is what this pins, for both launch modes.
    #[test]
    fn offscreen_chrome_cannot_reach_the_accessibility_bus() {
        for display in [None, Some(3)] {
            let (env, _) = chrome_env(display);
            let (key, addr) = &env[0];
            assert_eq!(key, "AT_SPI_BUS_ADDRESS");
            let path = addr
                .strip_prefix("unix:path=")
                .expect("must be a unix socket address so the connection simply fails");
            assert!(
                !std::path::Path::new(path).exists(),
                "{path} exists, so Chrome could reach a real bus through it"
            );
            // An absolute path, or Chrome would resolve it against its own cwd.
            assert!(path.starts_with('/'), "{path} must be absolute");
        }
    }

    /// Xvfb starts, says its display, and is gone once killed. Needs Xvfb.
    #[test]
    #[ignore]
    fn xvfb_starts_on_a_display_of_its_own_and_goes_away() {
        let (mut proc, display) = start_xvfb().expect("Xvfb starts");
        eprintln!("Xvfb took display :{display}");
        assert!(!proc.exited());
        proc.kill();
        assert!(proc.exited());
    }

    /// Every program started is one the manifest asks for, and nothing more
    /// is asked for than is started: the user approves exactly this list.
    #[test]
    fn the_manifest_grants_exactly_the_programs_started() {
        let manifest: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let granted: Vec<&str> = manifest["permissions"]["process"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        let started: Vec<&str> = CHROME_NAMES.iter().copied().chain([XVFB]).collect();
        assert_eq!(granted, started);
        assert_eq!(manifest["rendersPages"], true);
    }

    #[test]
    fn every_chrome_name_is_a_name_not_a_path() {
        for n in CHROME_NAMES.iter().chain([&XVFB]) {
            assert!(!n.contains('/') && !n.contains('\\'), "{n}");
        }
    }
}
