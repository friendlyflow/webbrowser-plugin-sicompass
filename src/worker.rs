//! The browser thread: one Chrome for the plugin's whole life.
//!
//! A call from the app has ten seconds, and a page load can take thirty, so
//! Chrome is driven from a thread of its own. The UI sends it [`Job`]s over a
//! channel and it answers each with a [`Done`], which the UI takes on its next
//! `poll`. A page rendered for another program's link goes straight to the app
//! from the thread (`host::page_rendered`).
//!
//! Jobs run one at a time, in the order they were sent. A navigation queued
//! behind a newer one is skipped: the user has already typed past it.
//!
//! The [`Worker`] handle owns the thread: dropping it closes Chrome, and stops
//! it (and its Xvfb) outright if the thread is busy with a load.

use crate::cdp::Children;
use crate::{LiveSession, PRUNE_HIDDEN};
use serde::{Deserialize, Serialize};
use sicompass_sdk::ffon::{FfonElement, FormMap, FormNode, FormNodeKind};
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::Duration;

/// How long the browser thread gets to close Chrome itself when the plugin
/// goes, before Chrome is stopped from outside. The app's runtime ends the
/// process two seconds after letting it go, so this and [`STOP_GRACE`] stay
/// well inside that.
const CLOSE_GRACE: Duration = Duration::from_millis(1200);

/// How long Chrome and Xvfb get to end after `SIGTERM`, before they are killed.
const STOP_GRACE: Duration = Duration::from_millis(400);

/// What the UI asks of the browser thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Job {
    /// Load `url` in the reader's tab. `seq` numbers the UI's navigations,
    /// so an answer for one the user has moved past is recognised.
    Navigate { seq: u64, url: String, prune: bool },
    /// Run a script that fills a form field in the reader's tab.
    Fill { js: String },
    /// Run a script that submits a form, then read the page it leads to.
    Submit {
        seq: u64,
        js: String,
        url: String,
        prune: bool,
    },
    /// Forget every cookie.
    ClearCookies,
    /// Render `url` for a link another program shows, in a tab of its own.
    Render { url: String, prune: bool },
    /// Close Chrome.
    Close,
}

/// What the browser thread answers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Done {
    /// A page for navigation `seq` (or, `submitted`, the page a form led to).
    Page {
        seq: u64,
        submitted: bool,
        page: WirePage,
    },
    /// Navigation `seq` failed: shown as the page, and on the status line.
    Failed { seq: u64, error: String },
    /// Something else failed: the status line only.
    Error { error: String },
    /// A page rendered for a link.
    Rendered { url: String, page: String },
}

/// A page and its forms, as they cross to the UI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WirePage {
    /// The FFON, as JSON.
    elements: String,
    forms: Vec<(String, WireFormNode)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireFormNode {
    css_selector: String,
    kind: WireKind,
    form_index: usize,
    match_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireKind {
    TextInput,
    Textarea,
    Checkbox,
    RadioOption { group: String, value: String },
    Submit,
    Select,
}

impl WirePage {
    pub fn new(elements: &[FfonElement], forms: &FormMap) -> Self {
        WirePage {
            elements: sicompass_sdk::ffon::to_json_string(elements).unwrap_or_default(),
            forms: forms
                .iter()
                .map(|(k, n)| {
                    let kind = match &n.kind {
                        FormNodeKind::TextInput => WireKind::TextInput,
                        FormNodeKind::Textarea => WireKind::Textarea,
                        FormNodeKind::Checkbox => WireKind::Checkbox,
                        FormNodeKind::RadioOption { group, value } => WireKind::RadioOption {
                            group: group.clone(),
                            value: value.clone(),
                        },
                        FormNodeKind::Submit => WireKind::Submit,
                        FormNodeKind::Select => WireKind::Select,
                    };
                    (
                        k.clone(),
                        WireFormNode {
                            css_selector: n.css_selector.clone(),
                            kind,
                            form_index: n.form_index,
                            match_index: n.match_index,
                        },
                    )
                })
                .collect(),
        }
    }

    pub fn into_parts(self) -> (Vec<FfonElement>, FormMap) {
        let elements = sicompass_sdk::ffon::parse_json(&self.elements).unwrap_or_default();
        let forms = self
            .forms
            .into_iter()
            .map(|(k, n)| {
                let kind = match n.kind {
                    WireKind::TextInput => FormNodeKind::TextInput,
                    WireKind::Textarea => FormNodeKind::Textarea,
                    WireKind::Checkbox => FormNodeKind::Checkbox,
                    WireKind::RadioOption { group, value } => {
                        FormNodeKind::RadioOption { group, value }
                    }
                    WireKind::Submit => FormNodeKind::Submit,
                    WireKind::Select => FormNodeKind::Select,
                };
                (
                    k,
                    FormNode {
                        css_selector: n.css_selector,
                        kind,
                        form_index: n.form_index,
                        match_index: n.match_index,
                    },
                )
            })
            .collect();
        (elements, forms)
    }
}

// ---------------------------------------------------------------------------
// The thread's side
// ---------------------------------------------------------------------------

/// What the browser thread holds between jobs: Chrome and the reader's tab.
#[derive(Default)]
pub struct WorkerState {
    live: Option<LiveSession>,
    /// Every program this browser started, for stopping them from outside.
    children: Children,
}

impl WorkerState {
    /// The live session, started if there is none (or Chrome went away).
    fn live(&mut self) -> Result<&mut LiveSession, String> {
        if let Some(live) = &mut self.live
            && !live.chrome.alive()
        {
            self.live = None;
        }
        if self.live.is_none() {
            self.live = Some(crate::init_live_session(&self.children)?);
        }
        Ok(self.live.as_mut().expect("started above"))
    }

    /// Close Chrome, if it runs.
    fn close(&mut self) {
        if let Some(live) = self.live.take() {
            live.chrome.close();
        }
    }

    /// Do one job. `None` for one that answers nothing.
    pub fn run(&mut self, job: Job) -> Option<Done> {
        match job {
            Job::Navigate { seq, url, prune } => {
                PRUNE_HIDDEN.store(prune, Ordering::Release);
                let live = match self.live() {
                    Ok(l) => l,
                    Err(e) => {
                        return Some(Done::Failed {
                            seq,
                            error: format!("Error launching browser: {e}"),
                        });
                    }
                };
                match crate::navigate_and_get_html(&mut live.chrome, &live.page, &url) {
                    Ok(load) => {
                        let (elements, forms) = crate::page_to_ffon_with_forms(&load, &url);
                        Some(Done::Page {
                            seq,
                            submitted: false,
                            page: WirePage::new(&elements, &forms),
                        })
                    }
                    Err(e) => {
                        // The next attempt starts from a fresh Chrome.
                        self.close();
                        Some(Done::Failed {
                            seq,
                            error: format!("Error loading {url}: {e}"),
                        })
                    }
                }
            }
            Job::Fill { js } => {
                if let Some(live) = &mut self.live {
                    let _ = live.chrome.evaluate(&live.page, &js, crate::secs(5));
                }
                None
            }
            Job::Submit {
                seq,
                js,
                url,
                prune,
            } => {
                PRUNE_HIDDEN.store(prune, Ordering::Release);
                let live = self.live.as_mut()?;
                let (chrome, page) = (&mut live.chrome, &live.page);
                let _ = chrome.evaluate(page, &js, crate::secs(5));
                // Wait for the page to settle after the click. A fixed guess
                // is not enough: a consent choice reloads in place and a submit
                // navigates, and at a desktop viewport bpost takes longer than
                // 2.5 s to put its content back.
                crate::await_stable_url(chrome, page, crate::secs(10));
                crate::await_page_settled(chrome, page, crate::secs(12));
                let html = crate::settled_html(chrome, page).ok()?;
                // Answering one step is what leads to the next: a language
                // choice usually lands on a page whose cookie banner has yet to
                // be answered. A submit can also land on a bot check just as a
                // navigation can, so either way the response goes through the
                // same renderer.
                let landed = chrome.url(page).unwrap_or(url);
                let (load, _) = crate::settle_gates(chrome, page, &landed, html);
                let (elements, forms) = crate::page_to_ffon_with_forms(&load, &landed);
                Some(Done::Page {
                    seq,
                    submitted: true,
                    page: WirePage::new(&elements, &forms),
                })
            }
            Job::ClearCookies => {
                if let Some(live) = &mut self.live {
                    return live
                        .chrome
                        .clear_cookies(&live.page)
                        .err()
                        .map(|_| Done::Error {
                            error: "Could not clear cookies (browser not responding)".to_owned(),
                        });
                }
                // No Chrome running: nothing in memory, clear the store on disk.
                let (parent, name) = crate::chrome_profile();
                crate::remove_cookie_files(&std::path::Path::new(&parent).join(name));
                None
            }
            Job::Render { url, prune } => {
                PRUNE_HIDDEN.store(prune, Ordering::Release);
                let page = match self.live() {
                    Ok(live) => crate::render_page(&mut live.chrome, &url),
                    Err(e) => vec![FfonElement::new_str(format!(
                        "Error launching browser: {e}"
                    ))],
                };
                Some(Done::Rendered {
                    page: sicompass_sdk::ffon::to_json_string(&page).unwrap_or_default(),
                    url,
                })
            }
            Job::Close => {
                self.close();
                None
            }
        }
    }
}

impl Drop for WorkerState {
    fn drop(&mut self) {
        self.close();
    }
}

/// Take the next job to run from `queue`, skipping a navigation that a
/// newer one queued behind it replaces.
fn next_job(queue: &mut VecDeque<Job>) -> Option<Job> {
    loop {
        let job = queue.pop_front()?;
        let superseded = matches!(job, Job::Navigate { .. })
            && queue.iter().any(|j| matches!(j, Job::Navigate { .. }));
        if !superseded {
            return Some(job);
        }
    }
}

/// The browser thread itself: jobs in, answers out, until the UI's handle
/// is dropped.
fn run_browser_thread(mut state: WorkerState, jobs: Receiver<Job>, done: Sender<Done>) {
    let mut queue = VecDeque::new();
    while let Ok(job) = jobs.recv() {
        queue.push_back(job);
        queue.extend(jobs.try_iter());
        while let Some(job) = next_job(&mut queue) {
            let closing = matches!(job, Job::Close);
            match state.run(job) {
                // A page for another program's link goes to the app, which
                // asked for it, without waiting for the UI's next poll.
                Some(Done::Rendered { url, page }) => {
                    let elements = sicompass_sdk::ffon::parse_json(&page).unwrap_or_default();
                    sicompass_sdk::plugin::host::page_rendered(&url, &elements);
                }
                Some(answer) => {
                    if done.send(answer).is_err() {
                        return;
                    }
                }
                None => {}
            }
            if closing {
                return;
            }
            queue.extend(jobs.try_iter());
        }
    }
}

// ---------------------------------------------------------------------------
// The UI's side
// ---------------------------------------------------------------------------

/// The UI's handle on the browser thread.
pub struct Worker {
    jobs: Sender<Job>,
    done: Receiver<Done>,
    /// Ends (disconnects) when the thread does.
    finished: Receiver<()>,
    /// What the thread started, stopped on drop whatever the thread is doing.
    children: Children,
}

impl Worker {
    pub fn start() -> Result<Worker, String> {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (done_tx, done) = mpsc::channel::<Done>();
        let (finished_tx, finished) = mpsc::channel::<()>();
        let state = WorkerState::default();
        let children = state.children.clone();
        std::thread::Builder::new()
            .name("webbrowser".to_owned())
            .spawn(move || {
                let _finished = finished_tx;
                run_browser_thread(state, job_rx, done_tx);
            })
            .map_err(|e| e.to_string())?;
        Ok(Worker {
            jobs,
            done,
            finished,
            children,
        })
    }

    /// A handle whose jobs go nowhere but the returned receiver, for tests
    /// of what the UI sends and how it takes answers, with no Chrome.
    #[cfg(test)]
    pub fn recording() -> (Worker, Receiver<Job>, Sender<Done>) {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (done_tx, done) = mpsc::channel::<Done>();
        let (_, finished) = mpsc::channel::<()>();
        let worker = Worker {
            jobs,
            done,
            finished,
            children: Children::default(),
        };
        (worker, job_rx, done_tx)
    }

    /// Hand the browser thread a job.
    pub fn send(&self, job: &Job) -> Result<(), String> {
        self.jobs
            .send(job.clone())
            .map_err(|_| "the browser has stopped".to_owned())
    }

    /// What the browser thread has finished since the last call, and `false`
    /// once the thread is gone (it will answer nothing more).
    pub fn drain(&self) -> (Vec<Done>, bool) {
        let mut out = Vec::new();
        loop {
            match self.done.try_recv() {
                Ok(d) => out.push(d),
                Err(TryRecvError::Empty) => return (out, true),
                Err(TryRecvError::Disconnected) => return (out, false),
            }
        }
    }
}

impl Drop for Worker {
    /// Close Chrome: the thread closes it itself when it is free to, and
    /// whatever still runs after [`CLOSE_GRACE`] (the thread is in the middle
    /// of a load) is stopped from here.
    fn drop(&mut self) {
        let _ = self.jobs.send(Job::Close);
        if !matches!(
            self.finished.recv_timeout(CLOSE_GRACE),
            Err(RecvTimeoutError::Disconnected)
        ) {
            crate::cdp::log("the browser was busy when the plugin closed; stopping Chrome");
        }
        self.children.stop_all(STOP_GRACE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nav(seq: u64) -> Job {
        Job::Navigate {
            seq,
            url: format!("https://{seq}.example/"),
            prune: true,
        }
    }

    #[test]
    fn a_navigation_queued_behind_a_newer_one_is_skipped() {
        let mut q: VecDeque<Job> = [nav(1), Job::ClearCookies, nav(2)].into();
        // 1 is replaced by 2; the cookie clear still runs, in order.
        assert!(matches!(next_job(&mut q), Some(Job::ClearCookies)));
        assert!(matches!(
            next_job(&mut q),
            Some(Job::Navigate { seq: 2, .. })
        ));
        assert!(next_job(&mut q).is_none());
    }

    #[test]
    fn a_page_and_its_forms_survive_the_trip() {
        let (elements, forms) = sicompass_sdk::ffon::html_to_ffon_with_forms(
            r#"<html><body><h1>Title</h1><form><input name="q" placeholder="Search">
               <input type="submit" value="Go"></form></body></html>"#,
            "https://example.com/",
        );
        let wire = WirePage::new(&elements, &forms);
        let done = Done::Page {
            seq: 3,
            submitted: false,
            page: wire,
        };
        let back: Done = serde_json::from_slice(&serde_json::to_vec(&done).unwrap()).unwrap();
        let Done::Page { seq, page, .. } = back else {
            panic!("not a page");
        };
        assert_eq!(seq, 3);
        let (e2, f2) = page.into_parts();
        assert_eq!(e2, elements);
        assert_eq!(f2, forms);
    }
}
