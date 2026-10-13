//! Driving Chrome over the DevTools protocol (CDP), blocking.
//!
//! On Unix, Chrome is started with `--remote-debugging-pipe`: it reads
//! protocol messages on file descriptor 3 and writes them on 4, each one JSON
//! ended by a NUL byte, and the pipes are made here. No network port is
//! opened, so nothing else on the machine can drive it, and Chrome exits when
//! the pipe closes, so it never outlives the plugin.
//!
//! Windows has no descriptors 3 and 4 to hand a program. There Chrome is
//! started with `--remote-debugging-port=0`: it picks a free port on
//! 127.0.0.1 and writes it to `DevToolsActivePort` in its profile, and the
//! protocol goes over a websocket to it (`tungstenite`, plain `ws://`).
//!
//! Only the handful of calls the browser makes are here: a tab
//! (`Target.createTarget`, attached with a flat session), navigation, script
//! evaluation, the viewport, cookies and closing. All of it runs on the
//! browser thread, one call at a time, so every wait is a poll of the channel
//! with a deadline.
//!
//! Every program started is registered in a [`Children`], so the plugin can
//! stop them from its own thread when sicompass lets it go, and each is
//! stopped when dropped.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, Weak};
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
/// program the plugin's `process` permission lists: on Linux the commands on
/// `PATH`, on macOS the applications (in `/Applications`), on Windows the
/// browsers in their install folders (see [`crate::program`]).
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

/// [`chrome_args`] for Windows: the protocol on a port of Chrome's choosing on
/// 127.0.0.1 instead of the pipe.
#[cfg_attr(not(windows), allow(dead_code))]
fn port_args(mut args: Vec<String>) -> Vec<String> {
    for a in &mut args {
        if a == "--remote-debugging-pipe" {
            *a = "--remote-debugging-port=0".to_owned();
        }
    }
    args
}

/// The port and the browser's websocket path, from `DevToolsActivePort`: the
/// port on the first line, the path on the second.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_devtools_active_port(text: &str) -> Option<(u16, String)> {
    let mut lines = text.lines();
    let port = lines
        .next()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)?;
    let path = lines.next()?.trim();
    path.starts_with("/devtools/browser/")
        .then(|| (port, path.to_owned()))
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
// The programs
// ---------------------------------------------------------------------------

type Shared = Arc<Mutex<Child>>;

/// Every program one browser started (Chrome, Xvfb), so they can all be
/// stopped from another thread than the one driving them: the plugin's
/// `cleanup` must not wait for a page load to finish before Chrome goes.
#[derive(Clone, Default)]
pub struct Children(Arc<Mutex<Vec<Weak<Mutex<Child>>>>>);

impl Children {
    fn add(&self, child: &Shared) {
        let mut list = self.0.lock().unwrap_or_else(|p| p.into_inner());
        list.retain(|w| w.strong_count() > 0);
        list.push(Arc::downgrade(child));
    }

    /// Stop every program still running: asked to end first (`SIGTERM`, so
    /// Chrome closes its own helpers and its profile), killed after `grace`.
    pub fn stop_all(&self, grace: Duration) {
        let live: Vec<Shared> = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for child in &live {
            ask_to_end(child);
        }
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline && live.iter().any(|c| !has_exited(c)) {
            std::thread::sleep(Duration::from_millis(20));
        }
        for child in &live {
            kill(child);
        }
    }

    /// How many of the programs still run.
    #[cfg(test)]
    fn running(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|c| !has_exited(c))
            .count()
    }
}

fn lock(child: &Shared) -> std::sync::MutexGuard<'_, Child> {
    child.lock().unwrap_or_else(|p| p.into_inner())
}

/// Whether the program has ended (and is reaped).
fn has_exited(child: &Shared) -> bool {
    !matches!(lock(child).try_wait(), Ok(None))
}

/// Ask the program to end. Only while it has not been reaped, so its pid is
/// still its own.
fn ask_to_end(child: &Shared) {
    let mut c = lock(child);
    if !matches!(c.try_wait(), Ok(None)) {
        return;
    }
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(c.id()) {
        // SAFETY: kill(2) with the pid of a child that has not been reaped.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    // Windows has no polite request for a windowless program.
    #[cfg(not(unix))]
    let _ = c.kill();
}

/// Kill the program and reap it. Nothing when it has ended already (`std`
/// remembers a reaped child and signals nothing).
fn kill(child: &Shared) {
    let mut c = lock(child);
    let _ = c.kill();
    let _ = c.wait();
}

/// Never leaving a Chrome behind on Windows, which has neither of the two nets
/// the other platforms get for free.
///
/// On Unix Chrome exits when its `--remote-debugging-pipe` closes, and on Linux
/// `PR_SET_PDEATHSIG` ends it with the thread that started it, so a plugin that
/// dies without running a destructor still takes Chrome with it. Windows speaks
/// the protocol over a TCP socket instead, and closing that means nothing to
/// Chrome: a plugin killed outright (the app crashed, the user ended the task)
/// left a headless Chrome running forever, holding the profile.
///
/// That mattered because a *second* Chrome on one `--user-data-dir` does not
/// start: it hands its command line to the one already there and exits at once,
/// writing no `DevToolsActivePort` and nothing on stderr. The browser then
/// reported "Chrome exited before it opened its DevTools port" on every page
/// load, for good, until the stray `chrome.exe` was ended by hand.
///
/// A job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is the Windows
/// equivalent: the handle lives in this process, so when this process ends,
/// however it ends, the OS closes it and kills everything in the job. Chrome's
/// own helper processes are in it too, which `Child::kill` alone never reached.
#[cfg(windows)]
mod job {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::sync::OnceLock;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            class: u32,
            info: *const c_void,
            len: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
    }

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    /// `JobObjectExtendedLimitInformation`.
    const EXTENDED_LIMIT_INFORMATION: u32 = 9;

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimitInformation {
        basic_limit_information: BasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    /// The handle is deliberately never closed: holding it for the life of the
    /// process is what arms kill-on-close.
    struct Job(*mut c_void);
    // SAFETY: a job handle is not bound to a thread, and nothing here mutates
    // through the pointer after creation.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    /// The one job every program this plugin starts is put in, made on first
    /// use. `None` when Windows refused, which only costs us the net.
    fn job() -> Option<&'static Job> {
        static JOB: OnceLock<Option<Job>> = OnceLock::new();
        JOB.get_or_init(|| {
            // SAFETY: an unnamed job with default security attributes.
            let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
            if handle.is_null() {
                return None;
            }
            let mut info = ExtendedLimitInformation::default();
            info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `info` is the struct `EXTENDED_LIMIT_INFORMATION` names,
            // and its own size is passed.
            let set = unsafe {
                SetInformationJobObject(
                    handle,
                    EXTENDED_LIMIT_INFORMATION,
                    (&raw const info).cast(),
                    size_of::<ExtendedLimitInformation>() as u32,
                )
            };
            (set != 0).then_some(Job(handle))
        })
        .as_ref()
    }

    /// Put `child` in the job, so it cannot outlive this process. Best effort:
    /// a failure leaves the child running normally, stopped by `Proc::drop` as
    /// before.
    pub fn adopt(child: &Child) {
        let Some(job) = job() else { return };
        // SAFETY: `child` has not been reaped, so its handle is open for as
        // long as this call.
        unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle().cast()) };
    }
}

/// Finding, and ending, a Chrome of ours that holds the profile when it left
/// no `DevToolsActivePort` to be asked through.
///
/// Every Chrome on a `--user-data-dir` owns a hidden `Chrome_MessageWindow`
/// named after that folder, which is how a second Chrome finds it, hands its
/// command line over and exits. Builds before `job` stranded Chromes whose port
/// file the next launch then deleted, so the window is the one thing left to
/// find them by. One found running for days on a real machine had exactly
/// this shape.
///
/// Only reached with the profile lock held (`lock_profile`, taken before every
/// launch), so no live tab of ours has a Chrome on this profile: whatever owns
/// the window is one that outlived its plugin. The folder is the plugin's own,
/// so no other program's Chrome is ever named after it.
#[cfg(windows)]
mod singleton {
    use std::ffi::{OsStr, c_void};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[link(name = "user32")]
    unsafe extern "system" {
        fn FindWindowW(class: *const u16, name: *const u16) -> *mut c_void;
        fn GetWindowThreadProcessId(window: *mut c_void, pid: *mut u32) -> u32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn TerminateProcess(process: *mut c_void, code: u32) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    const PROCESS_TERMINATE: u32 = 0x0001;
    const SYNCHRONIZE: u32 = 0x0010_0000;

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// The process holding `profile`'s singleton, if any. Window names compare
    /// case-insensitively, as Windows paths do.
    fn holder(profile: &Path) -> Option<u32> {
        let class = wide(OsStr::new("Chrome_MessageWindow"));
        let name = wide(profile.as_os_str());
        // SAFETY: both strings are NUL-terminated and outlive the call.
        let window = unsafe { FindWindowW(class.as_ptr(), name.as_ptr()) };
        if window.is_null() {
            return None;
        }
        let mut pid = 0;
        // SAFETY: `window` came from FindWindowW, and `pid` lives on this frame.
        unsafe { GetWindowThreadProcessId(window, &mut pid) };
        (pid != 0 && pid != std::process::id()).then_some(pid)
    }

    #[cfg(test)]
    pub fn holder_for_test(profile: &Path) -> Option<u32> {
        holder(profile)
    }

    /// Make sure no Chrome holds `profile`: give one that was just asked to
    /// close `grace` to go by itself, then end it, and wait for it to let go.
    /// True when there was one to end.
    pub fn end_the_chrome_holding(profile: &Path, grace: Duration) -> bool {
        let deadline = Instant::now() + grace;
        let pid = loop {
            match holder(profile) {
                None => return false,
                Some(pid) if Instant::now() >= deadline => break pid,
                Some(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        // SAFETY: handle calls on one pid; the handle is closed before return.
        unsafe {
            let process = OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid);
            if process.is_null() {
                return false;
            }
            TerminateProcess(process, 1);
            // Gone, and the singleton with it, before the next Chrome looks.
            WaitForSingleObject(process, 5_000);
            CloseHandle(process);
        }
        true
    }
}

/// A started program: Chrome, or Xvfb.
pub struct Proc {
    child: Shared,
    stdout: Option<Receiver<Vec<u8>>>,
    stderr: Option<Receiver<Vec<u8>>>,
}

/// Bytes read from `r` by a thread of its own, until it ends.
fn reader(mut r: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
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

fn drain(rx: &Option<Receiver<Vec<u8>>>) -> Vec<u8> {
    rx.as_ref()
        .map(|rx| rx.try_iter().flatten().collect())
        .unwrap_or_default()
}

/// The two ends a child talks the DevTools protocol on, as file descriptors
/// 3 (it reads) and 4 (it writes).
#[cfg(unix)]
struct ChildFds {
    reads: std::io::PipeReader,
    writes: std::io::PipeWriter,
}

impl Proc {
    /// Start `program` with `fds` (Unix) as its descriptors 3 and 4. It is
    /// added to `children`, and stopped when the `Proc` is dropped.
    fn start(
        program: &Path,
        args: &[String],
        env: &[(String, String)],
        unset: &[String],
        children: &Children,
        #[cfg(unix)] fds: Option<&ChildFds>,
    ) -> Result<Proc, String> {
        let mut cmd = sicompass_sdk::plugin::command(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for k in unset {
            cmd.env_remove(k);
        }
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::process::CommandExt;
            let fds = fds.map(|f| (f.reads.as_raw_fd(), f.writes.as_raw_fd()));
            // SAFETY: only async-signal-safe calls (fcntl, dup2, prctl) between
            // fork and exec.
            unsafe {
                cmd.pre_exec(move || {
                    // On Linux the program goes when the thread that started
                    // it does (the plugin's browser thread, or the plugin
                    // itself, killed): never a Chrome left behind.
                    #[cfg(target_os = "linux")]
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    if let Some((r, w)) = fds {
                        // Out of the way first, so 3 and 4 never overwrite
                        // each other's source. dup2 clears close-on-exec.
                        let r = libc::fcntl(r, libc::F_DUPFD, 10);
                        let w = libc::fcntl(w, libc::F_DUPFD, 10);
                        if r < 0 || w < 0 || libc::dup2(r, 3) < 0 || libc::dup2(w, 4) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start `{}`: {e}", program.display()))?;
        // Windows' stand-in for `PR_SET_PDEATHSIG` and the pipe that ends
        // Chrome when it closes, both of which it has not got (see `job`).
        // Right after `spawn`, so the browser process is in the job before it
        // has had time to take the profile's singleton.
        #[cfg(windows)]
        job::adopt(&child);
        let stdout = child.stdout.take().map(reader);
        let stderr = child.stderr.take().map(reader);
        let child = Arc::new(Mutex::new(child));
        children.add(&child);
        Ok(Proc {
            child,
            stdout,
            stderr,
        })
    }

    fn stdout(&mut self) -> Vec<u8> {
        drain(&self.stdout)
    }

    fn stderr(&mut self) -> Vec<u8> {
        drain(&self.stderr)
    }

    fn exited(&mut self) -> bool {
        has_exited(&self.child)
    }

    /// Wait up to `grace` for the program to end by itself.
    fn wait_for_exit(&mut self, grace: Duration) {
        let deadline = Instant::now() + grace;
        while !self.exited() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn kill(&mut self) {
        kill(&self.child);
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        // Never leave a Chrome (or its Xvfb) behind: the old browser tests
        // leaked one each and took the desktop down with them.
        self.kill();
    }
}

// ---------------------------------------------------------------------------
// The protocol's channel
// ---------------------------------------------------------------------------

/// Where the DevTools messages go and come from. Either way a message read is
/// JSON ended by a NUL byte, as on the pipe.
enum Channel {
    /// `--remote-debugging-pipe`: Chrome's descriptors 3 and 4 (Unix).
    #[cfg(unix)]
    Pipe {
        to_chrome: std::io::PipeWriter,
        from_chrome: Receiver<Vec<u8>>,
    },
    /// `--remote-debugging-port`: a websocket on 127.0.0.1, served by a
    /// thread of its own (Windows, which has no descriptors 3 and 4 to give).
    #[cfg_attr(not(windows), allow(dead_code))]
    Socket {
        to_chrome: Sender<String>,
        from_chrome: Receiver<Vec<u8>>,
    },
}

impl Channel {
    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        match self {
            #[cfg(unix)]
            Channel::Pipe { to_chrome, .. } => {
                std::io::Write::write_all(to_chrome, bytes).map_err(|e| e.to_string())
            }
            Channel::Socket { to_chrome, .. } => {
                let text = std::str::from_utf8(bytes.strip_suffix(&[0]).unwrap_or(bytes))
                    .map_err(|e| e.to_string())?;
                to_chrome
                    .send(text.to_owned())
                    .map_err(|_| "the connection to Chrome closed".to_owned())
            }
        }
    }

    fn receive(&mut self) -> Vec<u8> {
        match self {
            #[cfg(unix)]
            Channel::Pipe { from_chrome, .. } => from_chrome.try_iter().flatten().collect(),
            Channel::Socket { from_chrome, .. } => from_chrome.try_iter().flatten().collect(),
        }
    }

    /// Serve `ws` on a thread: messages sent go out, messages read come back
    /// NUL-ended. The thread closes the socket when the channel is dropped.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn socket(mut ws: tungstenite::WebSocket<std::net::TcpStream>) -> Result<Channel, String> {
        use tungstenite::{Error, Message};
        ws.get_mut()
            .set_read_timeout(Some(Duration::from_millis(10)))
            .map_err(|e| e.to_string())?;
        let (to_chrome, outgoing) = mpsc::channel::<String>();
        let (incoming, from_chrome) = mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name("webbrowser-cdp".to_owned())
            .spawn(move || {
                loop {
                    loop {
                        match outgoing.try_recv() {
                            Ok(text) => {
                                if ws.send(Message::text(text)).is_err() {
                                    return;
                                }
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => {
                                let _ = ws.close(None);
                                let _ = ws.flush();
                                return;
                            }
                        }
                    }
                    let bytes = match ws.read() {
                        Ok(Message::Text(t)) => t.as_bytes().to_vec(),
                        Ok(Message::Binary(b)) => b.to_vec(),
                        Ok(_) => continue,
                        Err(Error::Io(e))
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue;
                        }
                        Err(_) => return,
                    };
                    let mut msg = bytes;
                    msg.push(0);
                    if incoming.send(msg).is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Channel::Socket {
            to_chrome,
            from_chrome,
        })
    }
}

/// Whether `program` can be started: found by name (see [`crate::program`]).
fn available(program: &str) -> bool {
    crate::program::resolve(program).is_some()
}

/// Whether Chrome is put on a virtual X display when Xvfb is there. Not on
/// macOS, where Chrome draws with Cocoa whatever `DISPLAY` says, so a headed
/// Chrome would be a window on the user's screen. Not on Windows either.
const XVFB_POSSIBLE: bool = cfg!(all(unix, not(target_os = "macos")));

/// The first Chrome that can be started.
pub fn find_chrome() -> Option<PathBuf> {
    CHROME_NAMES.iter().find_map(|n| crate::program::resolve(n))
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
    channel: Channel,
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
    /// The Chrome program.
    pub chrome: &'a Path,
    /// Chrome's profile folder (`--user-data-dir`).
    pub profile: &'a Path,
    /// Where Chrome and its Xvfb are registered, for stopping them.
    pub children: &'a Children,
}

impl Chrome {
    /// Start Chrome: headed on a virtual display when Xvfb is there, which is
    /// what sites that turn headless Chrome away accept, and headless
    /// otherwise.
    pub fn launch(opts: &Launch) -> Result<Chrome, String> {
        Self::launch_with(opts, start_chrome)
    }

    /// [`Chrome::launch`], with the protocol channel made by `start`.
    fn launch_with(opts: &Launch, start: StartChrome) -> Result<Chrome, String> {
        let xvfb = if XVFB_POSSIBLE && available(XVFB) {
            match start_xvfb(opts.children) {
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
        let args = chrome_args(xvfb.is_none(), &opts.profile.to_string_lossy());
        let (proc, channel) = start(opts, args, &env, &unset)?;
        let mut chrome = Chrome {
            proc,
            channel,
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
                format!(
                    "Chrome ({}) did not start: {e} {tail}",
                    opts.chrome.display()
                )
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

    /// Close Chrome, and the display it was on: asked to close, so it saves
    /// its profile, and killed if it has not gone a second later.
    pub fn close(mut self) {
        if self
            .call(None, "Browser.close", json!({}), Duration::from_millis(500))
            .is_ok()
        {
            self.proc.wait_for_exit(Duration::from_secs(1));
        }
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
        self.channel
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
        let got = self.channel.receive();
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

/// How Chrome is started on a protocol channel.
type StartChrome =
    fn(&Launch, Vec<String>, &[(String, String)], &[String]) -> Result<(Proc, Channel), String>;

/// Start Chrome on its protocol channel: the pipe on Unix.
#[cfg(unix)]
fn start_chrome(
    opts: &Launch,
    args: Vec<String>,
    env: &[(String, String)],
    unset: &[String],
) -> Result<(Proc, Channel), String> {
    let (reads, to_chrome) = std::io::pipe().map_err(|e| e.to_string())?;
    let (from_chrome, writes) = std::io::pipe().map_err(|e| e.to_string())?;
    let fds = ChildFds { reads, writes };
    let proc = Proc::start(opts.chrome, &args, env, unset, opts.children, Some(&fds))?;
    // Chrome's ends are Chrome's now: with ours closed, the pipe ends when
    // either side goes.
    drop(fds);
    let channel = Channel::Pipe {
        to_chrome,
        from_chrome: reader(from_chrome),
    };
    Ok((proc, channel))
}

/// Start Chrome on its protocol channel: a websocket on Windows.
#[cfg(not(unix))]
fn start_chrome(
    opts: &Launch,
    args: Vec<String>,
    env: &[(String, String)],
    unset: &[String],
) -> Result<(Proc, Channel), String> {
    start_chrome_on_a_port(opts, args, env, unset)
}

/// Start Chrome with the protocol on a port of its choosing, and connect to
/// it: the way on Windows, and testable anywhere.
#[cfg_attr(not(windows), allow(dead_code))]
fn start_chrome_on_a_port(
    opts: &Launch,
    args: Vec<String>,
    env: &[(String, String)],
    unset: &[String],
) -> Result<(Proc, Channel), String> {
    let port_file = opts.profile.join("DevToolsActivePort");
    // Read before it is removed: a Chrome of ours stranded by a plugin that
    // was killed outright still answers on the port it recorded there, and it
    // has to go before this one can start (see `close_a_stranded_chrome`).
    // `job` keeps that from happening again, but every Chrome left behind by a
    // build without it is still out there holding the profile.
    let recorded = std::fs::read_to_string(&port_file)
        .ok()
        .and_then(|t| parse_devtools_active_port(&t));
    let asked = recorded.is_some();
    if let Some((port, path)) = recorded {
        close_a_stranded_chrome(port, &path);
    }
    // And one with no port file left to ask through, or that did not go when
    // asked, by the window Chrome keeps for its profile. A Chrome just asked to
    // close gets a moment first, so it can save the user's cookies.
    #[cfg(windows)]
    if singleton::end_the_chrome_holding(
        opts.profile,
        if asked {
            Duration::from_secs(3)
        } else {
            Duration::ZERO
        },
    ) {
        log("ended a Chrome left behind on the browser profile");
    }
    #[cfg(not(windows))]
    let _ = asked;
    // One left by an earlier Chrome names a port nobody listens on.
    let _ = std::fs::remove_file(&port_file);
    let mut proc = Proc::start(
        opts.chrome,
        &port_args(args),
        env,
        unset,
        opts.children,
        #[cfg(unix)]
        None,
    )?;
    let deadline = Instant::now() + LAUNCH_TIMEOUT;
    let (port, path) = loop {
        if let Some(found) = std::fs::read_to_string(&port_file)
            .ok()
            .and_then(|t| parse_devtools_active_port(&t))
        {
            break found;
        }
        if proc.exited() {
            return Err(format!(
                "Chrome exited before it opened its DevTools port{}",
                why_it_might_have_exited(&mut proc)
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Chrome opened no DevTools port{}",
                why_it_might_have_exited(&mut proc)
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stream = std::net::TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("cannot reach Chrome's DevTools port: {e}"))?;
    let _ = stream.set_nodelay(true);
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(512 << 20))
        .max_frame_size(Some(512 << 20));
    let (ws, _) = tungstenite::client::client_with_config(
        format!("ws://127.0.0.1:{port}{path}"),
        stream,
        Some(config),
    )
    .map_err(|e| format!("Chrome's DevTools websocket: {e}"))?;
    Ok((proc, Channel::socket(ws)?))
}

/// What to add to a "Chrome did not open its port" message. Chrome says why on
/// its stderr when it has a reason (an unwritable profile, a switch it refuses),
/// and says nothing at all when it merely handed its command line to a Chrome
/// already on the profile, so the absence is worth reading too.
#[cfg_attr(not(windows), allow(dead_code))]
fn why_it_might_have_exited(proc: &mut Proc) -> String {
    let stderr = String::from_utf8_lossy(&proc.stderr()).into_owned();
    let tail: Vec<&str> = stderr
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .collect();
    if tail.is_empty() {
        // Nothing on stderr and gone at once is what the profile's singleton
        // looks like from here, so name it: it is the one cause the user can
        // do something about.
        " and said nothing, which is what a Chrome already running on this \
         profile looks like. Close any leftover Chrome and try again."
            .to_owned()
    } else {
        format!(
            ": {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        )
    }
}

/// Ask a Chrome of ours that outlived its plugin to close, so this one can have
/// the profile. `port` and `path` are what it recorded in `DevToolsActivePort`.
///
/// Best effort and quiet: the port may be free, taken by something else, or
/// held by a Chrome that is already going. Nothing here is fatal, because the
/// launch that follows reports the real failure anyway. Only a peer that
/// completes a DevTools websocket handshake on the path Chrome wrote is spoken
/// to, and that file lives inside our own profile folder.
#[cfg_attr(not(windows), allow(dead_code))]
fn close_a_stranded_chrome(port: u16, path: &str) {
    let Ok(stream) = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    ) else {
        return;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let Ok((mut ws, _)) =
        tungstenite::client::client(format!("ws://127.0.0.1:{port}{path}"), stream)
    else {
        return;
    };
    log("closing a Chrome left behind on the browser profile");
    let _ = ws.send(tungstenite::Message::Text(
        json!({"id": 1, "method": "Browser.close", "params": {}})
            .to_string()
            .into(),
    ));
    let _ = ws.flush();
    // It stops answering as it goes; a read is only to let it get there.
    let _ = ws.read();
    let _ = ws.close(None);
}

/// Start Xvfb on a display of its choosing (`-displayfd 1`: it writes the
/// number it took to its stdout), and wait for that number.
fn start_xvfb(children: &Children) -> Result<(Proc, u32), String> {
    let program = crate::program::resolve(XVFB).ok_or("Xvfb was not found")?;
    let mut proc = Proc::start(
        &program,
        &xvfb_args(),
        &[],
        &[],
        children,
        #[cfg(unix)]
        None,
    )?;
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

pub(crate) fn log(msg: &str) {
    sicompass_sdk::plugin::host::log(&format!("webbrowser: {msg}"));
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
        let (mut proc, display) = start_xvfb(&Children::default()).expect("Xvfb starts");
        eprintln!("Xvfb took display :{display}");
        assert!(!proc.exited());
        proc.kill();
        assert!(proc.exited());
    }

    /// Every program started is one the manifest names, and nothing more is
    /// named than is started: the user is shown exactly this list.
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

    /// Windows speaks the protocol on a port: the pipe flag is swapped for
    /// it, and nothing else changes.
    #[test]
    fn the_port_flavour_swaps_only_the_pipe() {
        let pipe = chrome_args(true, "p");
        let port = port_args(pipe.clone());
        assert!(port.contains(&"--remote-debugging-port=0".to_owned()));
        assert!(!port.contains(&"--remote-debugging-pipe".to_owned()));
        assert_eq!(pipe.len(), port.len());
    }

    #[test]
    fn devtools_active_port_is_read_as_a_port_and_a_browser_path() {
        assert_eq!(
            parse_devtools_active_port("41235\n/devtools/browser/abc-123\n"),
            Some((41235, "/devtools/browser/abc-123".to_owned()))
        );
        // Half-written, or not Chrome's.
        assert_eq!(parse_devtools_active_port("41235\n"), None);
        assert_eq!(parse_devtools_active_port(""), None);
        assert_eq!(parse_devtools_active_port("0\n/devtools/browser/x"), None);
        assert_eq!(parse_devtools_active_port("41235\n/json"), None);
    }

    /// What the plugin's cleanup relies on: every program a browser started
    /// can be stopped from another thread, including one that ignores the
    /// polite request.
    #[cfg(unix)]
    #[test]
    fn every_child_is_stopped_even_one_that_ignores_sigterm() {
        let children = Children::default();
        let sh = Path::new("/bin/sh");
        let start = |script: &str| {
            Proc::start(
                sh,
                &["-c".to_owned(), script.to_owned()],
                &[],
                &[],
                &children,
                None,
            )
            .expect("sh starts")
        };
        let polite = start("exec sleep 30");
        let stubborn = start("trap '' TERM; exec sleep 30");
        assert_eq!(children.running(), 2);
        let t = Instant::now();
        children.stop_all(Duration::from_millis(300));
        assert_eq!(children.running(), 0);
        assert!(t.elapsed() < Duration::from_secs(5));
        drop((polite, stubborn));
    }

    /// A dropped program does not run on.
    #[cfg(unix)]
    #[test]
    fn a_dropped_proc_is_stopped() {
        let children = Children::default();
        let proc = Proc::start(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "exec sleep 30".to_owned()],
            &[],
            &[],
            &children,
            None,
        )
        .expect("sh starts");
        let child = proc.child.clone();
        drop(proc);
        assert!(has_exited(&child));
    }

    /// The Windows way (a port and a websocket), run here against a real
    /// Chrome: it starts, answers, and goes. Needs Chrome.
    #[test]
    #[ignore]
    fn chrome_answers_over_its_devtools_port() {
        let chrome = find_chrome().expect("Chrome is installed");
        let profile = tempfile::tempdir().unwrap();
        let children = Children::default();
        let opts = Launch {
            chrome: &chrome,
            profile: profile.path(),
            children: &children,
        };
        let mut c = Chrome::launch_with(&opts, start_chrome_on_a_port).expect("Chrome starts");
        let page = c.new_page().expect("a tab");
        let v = c
            .evaluate(&page, "6 * 7", Duration::from_secs(10))
            .expect("evaluates");
        assert_eq!(v, json!(42));
        c.close();
        assert_eq!(children.running(), 0, "Chrome and its Xvfb are gone");
    }

    /// A Chrome that outlived its plugin does not block the next one.
    ///
    /// This is the Windows failure the job object now prevents and
    /// `close_a_stranded_chrome` still has to clear up after: a second Chrome
    /// on one profile hands its command line over and exits at once, so
    /// without the reclaim the second launch here fails with "Chrome exited
    /// before it opened its DevTools port". The first Chrome is leaked on
    /// purpose, the way a killed plugin leaks one; the job object ends it with
    /// this test process. Needs Chrome.
    #[test]
    #[ignore]
    fn a_chrome_left_on_the_profile_is_closed_so_the_next_one_can_start() {
        let chrome = find_chrome().expect("Chrome is installed");
        let profile = tempfile::tempdir().unwrap();
        let children = Children::default();
        let opts = Launch {
            chrome: &chrome,
            profile: profile.path(),
            children: &children,
        };

        let stranded = Chrome::launch_with(&opts, start_chrome_on_a_port).expect("Chrome starts");
        assert!(
            profile.path().join("DevToolsActivePort").exists(),
            "Chrome recorded the port the reclaim reads"
        );
        // Not dropped and not closed: exactly what a plugin killed outright
        // leaves behind.
        std::mem::forget(stranded);

        let mut next = Chrome::launch_with(&opts, start_chrome_on_a_port)
            .expect("the second Chrome starts once the stranded one is closed");
        let page = next.new_page().expect("a tab");
        let v = next
            .evaluate(&page, "6 * 7", Duration::from_secs(10))
            .expect("evaluates");
        assert_eq!(v, json!(42));
        next.close();
    }

    /// The same, for a Chrome whose `DevToolsActivePort` is already gone, so
    /// there is no port to ask it through: what builds before the job object
    /// left behind, because each later launch deleted the file first. It is
    /// found by its profile's singleton window instead. Needs Chrome.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn a_chrome_left_with_no_port_file_is_found_by_its_window() {
        let chrome = find_chrome().expect("Chrome is installed");
        let profile = tempfile::tempdir().unwrap();
        let children = Children::default();
        let opts = Launch {
            chrome: &chrome,
            profile: profile.path(),
            children: &children,
        };

        let stranded = Chrome::launch_with(&opts, start_chrome_on_a_port).expect("Chrome starts");
        std::mem::forget(stranded);
        std::fs::remove_file(profile.path().join("DevToolsActivePort"))
            .expect("the port file is there to delete");
        assert!(
            singleton::holder_for_test(profile.path()).is_some(),
            "the stranded Chrome is found by its window"
        );

        let mut next = Chrome::launch_with(&opts, start_chrome_on_a_port)
            .expect("the second Chrome starts once the stranded one is ended");
        let page = next.new_page().expect("a tab");
        let v = next
            .evaluate(&page, "6 * 7", Duration::from_secs(10))
            .expect("evaluates");
        assert_eq!(v, json!(42));
        next.close();
    }

    /// The pipe, against a real Chrome, and stopped from another thread the
    /// way the plugin's cleanup does it. Needs Chrome.
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn chrome_on_its_pipe_is_stopped_from_another_thread() {
        let chrome = find_chrome().expect("Chrome is installed");
        let profile = tempfile::tempdir().unwrap();
        let children = Children::default();
        let opts = Launch {
            chrome: &chrome,
            profile: profile.path(),
            children: &children,
        };
        let mut c = Chrome::launch(&opts).expect("Chrome starts");
        assert!(c.alive());
        let stopper = children.clone();
        std::thread::spawn(move || stopper.stop_all(Duration::from_secs(1)))
            .join()
            .unwrap();
        assert!(!c.alive());
        assert_eq!(children.running(), 0);
    }

    #[test]
    fn every_chrome_name_is_a_name_not_a_path() {
        for n in CHROME_NAMES.iter().chain([&XVFB]) {
            assert!(!n.contains('/') && !n.contains('\\'), "{n}");
        }
    }
}
