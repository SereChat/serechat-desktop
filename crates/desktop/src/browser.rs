//! The agent's browser: an installed Chrome-family browser (the one picked
//! in Settings, or the first of [`KINDS`] found) that the app starts with a fresh, throwaway profile (no saved logins, cookies
//! or history of the user's) and drives over its remote debugging protocol.
//!
//! One browser serves the whole app; each conversation gets a tab of its
//! own. The window is visible, so the user can watch, and step in. The
//! browser starts on first use and quits when the app exits or the last
//! conversation using it is deleted. If the user closes the browser or a
//! tab, the next call opens it again.
//!
//! Pages are read as text outlines (`browser.js`), with links, buttons and
//! fields numbered; the agent clicks and types by number.
//!
//! Calls block; the tools run them on worker threads. They take turns.
//!
//! ponytail: one lock for the whole browser, so conversations take turns
//! (a lock per tab if several agents browse at once). Snap-packaged
//! Chromium cannot see a profile in the temporary folder; it is found last.

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::process::kill_tree;
use crate::websocket::{STOPPED, Socket};

/// Outlines a page; see the file.
const SNAPSHOT: &str = include_str!("browser.js");
/// The numbered elements of the latest snapshot, as a JavaScript expression.
const REFS: &str = "(window.__serechatRefs || [])";
/// Longest a single debugging command may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest the browser may take to start.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest the agent waits for a page to load before reading it anyway.
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);

static BROWSER: Mutex<Option<Browser>> = Mutex::new(None);
/// Key of the browser picked in Settings; `None` for Auto.
static CHOICE: Mutex<Option<String>> = Mutex::new(None);

/// A supported browser and where it installs on each platform.
struct Kind {
    /// Stored in the config.
    key: &'static str,
    /// Shown in Settings.
    name: &'static str,
    /// Paths under `ProgramFiles`, `ProgramFiles(x86)` and `LOCALAPPDATA`.
    windows: &'static str,
    /// Path under `/Applications` and `~/Applications`.
    macos: &'static str,
    /// Commands on `PATH`.
    linux: &'static [&'static str],
}

/// The supported browsers, in the order Auto prefers them.
const KINDS: [Kind; 5] = [
    Kind {
        key: "chrome",
        name: "Google Chrome",
        windows: r"Google\Chrome\Application\chrome.exe",
        macos: "Google Chrome.app/Contents/MacOS/Google Chrome",
        linux: &["google-chrome", "google-chrome-stable"],
    },
    Kind {
        key: "brave",
        name: "Brave",
        windows: r"BraveSoftware\Brave-Browser\Application\brave.exe",
        macos: "Brave Browser.app/Contents/MacOS/Brave Browser",
        linux: &["brave-browser", "brave"],
    },
    Kind {
        key: "helium",
        name: "Helium",
        windows: r"imput\Helium\Application\chrome.exe",
        macos: "Helium.app/Contents/MacOS/Helium",
        linux: &["helium", "helium-browser"],
    },
    Kind {
        key: "edge",
        name: "Microsoft Edge",
        windows: r"Microsoft\Edge\Application\msedge.exe",
        macos: "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        linux: &["microsoft-edge", "microsoft-edge-stable"],
    },
    Kind {
        key: "chromium",
        name: "Chromium",
        windows: r"Chromium\Application\chrome.exe",
        macos: "Chromium.app/Contents/MacOS/Chromium",
        linux: &["chromium", "chromium-browser"],
    },
];

/// A supported browser found on this computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    /// Stored in the config to pick it.
    pub key: &'static str,
    /// Its name, for Settings.
    pub name: &'static str,
    /// Its executable.
    pub path: PathBuf,
}

fn browser() -> MutexGuard<'static, Option<Browser>> {
    BROWSER.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A conversation's tab.
struct Tab {
    /// The conversation.
    owner: u64,
    /// Id of the tab in the debugging protocol.
    target: String,
    /// The session commands for it go through, while attached on the
    /// current connection.
    session: Option<String>,
}

/// The running browser. Dropping it quits the browser and deletes its profile.
struct Browser {
    child: Child,
    /// The executable it was started from.
    program: PathBuf,
    /// Throwaway profile folder.
    profile: PathBuf,
    /// Debugging port on 127.0.0.1 and the browser's WebSocket path.
    port: u16,
    path: String,
    /// Connection, opened on first use and after a failure.
    socket: Option<Socket>,
    next_id: u64,
    tabs: Vec<Tab>,
    /// The tab the browser opened with, handed to the first conversation.
    spare: Option<String>,
}

impl Drop for Browser {
    fn drop(&mut self) {
        kill_tree(&mut self.child);
        // The browser's helpers may hold the profile for a moment after it exits.
        for _ in 0..20 {
            if fs::remove_dir_all(&self.profile).is_ok() || !self.profile.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Browser {
    /// Starts an installed browser and waits until it takes debugging connections.
    fn launch(program: PathBuf, cancel: &AtomicBool) -> Result<Self, String> {
        let profile = std::env::temp_dir().join(format!("serechat-browser-{}", std::process::id()));
        // Left by a browser this run started before.
        let _ = fs::remove_dir_all(&profile);
        fs::create_dir_all(&profile).map_err(|e| format!("The browser profile could not be created: {e}"))?;
        let mut command = Command::new(&program);
        command
            // Port 0: the browser picks a free port and writes it to DevToolsActivePort.
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.display()))
            .args(["--no-first-run", "--no-default-browser-check", "--disable-sync", "--window-size=1280,900", "about:blank"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let child = command.spawn().map_err(|e| format!("{} could not start: {e}", program.display()))?;
        // From here on, dropping it cleans up.
        let mut browser = Self { child, program, profile, port: 0, path: String::new(), socket: None, next_id: 0, tabs: Vec::new(), spare: None };
        let started = Instant::now();
        loop {
            if let Some((port, path)) = fs::read_to_string(browser.profile.join("DevToolsActivePort")).ok().as_deref().and_then(parse_active_port) {
                (browser.port, browser.path) = (port, path);
                break;
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(STOPPED.into());
            }
            if !matches!(browser.child.try_wait(), Ok(None)) {
                return Err(format!("{} closed as soon as it started.", browser.program.display()));
            }
            if started.elapsed() > START_TIMEOUT {
                return Err(format!("{} did not start in time.", browser.program.display()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        browser.spare = browser.pages(cancel)?.into_iter().next();
        Ok(browser)
    }

    /// Whether the browser process is still running.
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Sends debugging command `method` (to tab `session`, or the browser
    /// itself) and returns its result. After a connection failure the next
    /// call reconnects.
    fn call(&mut self, session: Option<&str>, method: &str, params: Value, cancel: &AtomicBool) -> Result<Value, String> {
        let socket = match self.socket.take() {
            Some(socket) => socket,
            None => Socket::connect(self.port, &self.path, cancel)?,
        };
        let socket = self.socket.insert(socket);
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({ "id": id, "method": method });
        message["params"] = params;
        if let Some(session) = session {
            message["sessionId"] = session.into();
        }
        let reply = match exchange(socket, &message.to_string(), id, cancel) {
            Ok(reply) => reply,
            Err(e) => {
                // A message may be half read: start over, and re-attach the
                // tabs, since sessions belong to the connection.
                self.socket = None;
                self.tabs.iter_mut().for_each(|tab| tab.session = None);
                return Err(e);
            }
        };
        if let Some(error) = reply.get("error") {
            let message = error.get("message").and_then(Value::as_str).unwrap_or("The browser refused the command.");
            return Err(format!("The browser refused {method}: {message}"));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    /// The ids of the open tabs.
    fn pages(&mut self, cancel: &AtomicBool) -> Result<Vec<String>, String> {
        let targets = self.call(None, "Target.getTargets", json!({}), cancel)?;
        let infos = targets.get("targetInfos").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
        Ok(infos
            .iter()
            .filter(|info| info.get("type").and_then(Value::as_str) == Some("page"))
            .filter_map(|info| info.get("targetId").and_then(Value::as_str).map(str::to_owned))
            .collect())
    }

    /// The session of `owner`'s tab, brought to the front; opens the tab or
    /// re-attaches to it as needed.
    fn attach(&mut self, owner: u64, cancel: &AtomicBool) -> Result<String, String> {
        let known = self.tabs.iter().find(|tab| tab.owner == owner).map(|tab| (tab.target.clone(), tab.session.clone()));
        if let Some((_, Some(session))) = &known {
            // Fails if the user closed the tab or the connection was reset.
            if self.call(Some(session), "Page.bringToFront", json!({}), cancel).is_ok() {
                return Ok(session.clone());
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(STOPPED.into());
            }
        }
        let open = self.pages(cancel)?;
        // The conversation's tab if still open, else the browser's first tab, else a new one.
        let reusable = known.map(|(target, _)| target).filter(|t| open.contains(t)).or_else(|| self.spare.take().filter(|t| open.contains(t)));
        let target = if let Some(target) = reusable {
            target
        } else {
            let created = self.call(None, "Target.createTarget", json!({ "url": "about:blank" }), cancel)?;
            created.get("targetId").and_then(Value::as_str).ok_or("The browser did not open a tab.")?.to_owned()
        };
        let attached = self.call(None, "Target.attachToTarget", json!({ "targetId": target, "flatten": true }), cancel)?;
        let session = attached.get("sessionId").and_then(Value::as_str).ok_or("The browser did not attach to the tab.")?.to_owned();
        self.tabs.retain(|tab| tab.owner != owner);
        self.tabs.push(Tab { owner, target, session: Some(session.clone()) });
        self.call(Some(&session), "Page.bringToFront", json!({}), cancel)?;
        Ok(session)
    }
}

/// Sends `message` and waits for the reply with `id`, skipping events and
/// replies to calls given up on.
fn exchange(socket: &mut Socket, message: &str, id: u64, cancel: &AtomicBool) -> Result<Value, String> {
    socket.send(message)?;
    let deadline = Instant::now() + CALL_TIMEOUT;
    loop {
        let text = socket.receive(deadline.saturating_duration_since(Instant::now()), cancel)?;
        if let Ok(reply) = serde_json::from_str::<Value>(&text)
            && reply.get("id").and_then(Value::as_u64) == Some(id)
        {
            return Ok(reply);
        }
    }
}

/// A conversation's tab, ready for commands.
struct Page<'a> {
    browser: &'a mut Browser,
    session: String,
    cancel: &'a AtomicBool,
}

impl Page<'_> {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.browser.call(Some(&self.session), method, params, self.cancel)
    }

    /// Evaluates JavaScript `expression` in the page and returns its value.
    fn evaluate(&mut self, expression: &str) -> Result<Value, String> {
        let result = self.call("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true }))?;
        if let Some(details) = result.get("exceptionDetails") {
            let text = details.pointer("/exception/description").or_else(|| details.get("text")).and_then(Value::as_str);
            return Err(format!("The page's script failed: {}", text.unwrap_or("unknown error")));
        }
        Ok(result.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    /// The page as a text outline with numbered elements.
    fn snapshot(&mut self) -> Result<String, String> {
        self.evaluate(SNAPSHOT)?.as_str().map(str::to_owned).ok_or_else(|| "The page could not be read.".into())
    }

    /// Waits for the page to finish loading after an action that may have
    /// started a navigation. A page still loading after [`LOAD_TIMEOUT`] is
    /// read as it is.
    fn settle(&mut self) -> Result<(), String> {
        // Gives a click or Enter time to start navigating.
        pause(Duration::from_millis(300), self.cancel)?;
        let started = Instant::now();
        while started.elapsed() < LOAD_TIMEOUT {
            match self.evaluate("document.readyState") {
                Ok(state) if state == "complete" => return Ok(()),
                // A lost connection won't come back by waiting.
                Err(e) if self.browser.socket.is_none() => return Err(e),
                // Evaluating fails while one document replaces another.
                _ => pause(Duration::from_millis(100), self.cancel)?,
            }
        }
        Ok(())
    }

    /// The viewport centre of element `index` (from 1) of the latest
    /// snapshot, scrolled into view.
    fn locate(&mut self, index: u64) -> Result<(f64, f64), String> {
        let point = self.evaluate(&format!(
            "(() => {{ const el = {REFS}[{}]; if (!el || !el.isConnected) return null; \
             el.scrollIntoView({{ block: 'center', inline: 'center' }}); const r = el.getBoundingClientRect(); \
             return [r.left + r.width / 2, r.top + r.height / 2]; }})()",
            slot(index)?
        ))?;
        match point.as_array().map(Vec::as_slice) {
            Some([x, y]) => x.as_f64().zip(y.as_f64()).ok_or_else(|| stale(index)),
            _ => Err(stale(index)),
        }
    }
}

/// The position in the snapshot's list of element number `index`.
fn slot(index: u64) -> Result<u64, String> {
    index.checked_sub(1).ok_or_else(|| "Element numbers start at 1.".to_owned())
}

/// The error for an element number that no longer applies.
fn stale(index: u64) -> String {
    format!("Element {index} is not on the page any more. Take a new snapshot with browser_snapshot and use its numbers.")
}

/// Sleeps for `duration` unless `cancel` is raised first.
fn pause(duration: Duration, cancel: &AtomicBool) -> Result<(), String> {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        if cancel.load(Ordering::Relaxed) {
            return Err(STOPPED.into());
        }
        std::thread::sleep(Duration::from_millis(25).min(end.saturating_duration_since(Instant::now())));
    }
    Ok(())
}

/// Runs `act` on conversation `owner`'s tab, starting the browser and
/// opening the tab first if needed.
fn with_page<T>(owner: u64, cancel: &AtomicBool, act: impl FnOnce(&mut Page<'_>) -> Result<T, String>) -> Result<T, String> {
    let program = chosen(installed(), choice().as_deref())
        .ok_or("No supported browser was found. Install Google Chrome, Brave, Helium, Microsoft Edge or Chromium to let the agent browse.")?
        .path;
    let mut guard = browser();
    // The user closed the browser, or picked another in Settings: start anew.
    if guard.as_mut().is_some_and(|browser| !browser.alive() || browser.program != program) {
        *guard = None;
    }
    let browser = match guard.take() {
        Some(browser) => guard.insert(browser),
        None => guard.insert(Browser::launch(program, cancel)?),
    };
    let session = browser.attach(owner, cancel)?;
    act(&mut Page { browser, session, cancel })
}

/// Opens `url` in `owner`'s tab and returns the page's outline.
///
/// # Errors
/// Not an http(s) URL, no browser, or the page could not be opened.
pub fn open(owner: u64, url: &str, cancel: &AtomicBool) -> Result<String, String> {
    let scheme = url.split_once("://").map(|(scheme, _)| scheme.to_ascii_lowercase());
    if !matches!(scheme.as_deref(), Some("http" | "https")) {
        return Err("Only http:// and https:// URLs can be opened.".into());
    }
    with_page(owner, cancel, |page| {
        let result = page.call("Page.navigate", json!({ "url": url }))?;
        if let Some(error) = result.get("errorText").and_then(Value::as_str).filter(|e| !e.is_empty()) {
            return Err(format!("{url} could not be opened: {error}"));
        }
        page.settle()?;
        page.snapshot()
    })
}

/// The outline of `owner`'s current page.
///
/// # Errors
/// No browser, or the page could not be read.
#[expect(clippy::redundant_closure_for_method_calls, reason = "`Page::snapshot` alone is not general over the page's lifetime")]
pub fn snapshot(owner: u64, cancel: &AtomicBool) -> Result<String, String> {
    with_page(owner, cancel, |page| page.snapshot())
}

/// Clicks element `index` of the latest snapshot like a mouse would, then
/// returns the page's new outline.
///
/// # Errors
/// The element is gone, or the browser failed.
pub fn click(owner: u64, index: u64, cancel: &AtomicBool) -> Result<String, String> {
    with_page(owner, cancel, |page| {
        let (x, y) = page.locate(index)?;
        for (kind, button) in [("mouseMoved", "none"), ("mousePressed", "left"), ("mouseReleased", "left")] {
            page.call("Input.dispatchMouseEvent", json!({ "type": kind, "x": x, "y": y, "button": button, "clickCount": 1 }))?;
        }
        page.settle()?;
        page.snapshot()
    })
}

/// Types `text` into element `index` of the latest snapshot (replacing its
/// content if `clear`), or picks the dropdown option labelled `text`;
/// presses Enter afterwards if `submit`. Returns the page's new outline.
///
/// # Errors
/// The element is gone, takes no text, has no such option, or the browser failed.
pub fn type_text(owner: u64, index: u64, text: &str, clear: bool, submit: bool, cancel: &AtomicBool) -> Result<String, String> {
    with_page(owner, cancel, |page| {
        // Embedded as a JSON string, which is a valid JavaScript string literal.
        let wanted = Value::from(text.trim().to_lowercase());
        let outcome = page.evaluate(&format!(
            "(() => {{ const el = {REFS}[{}]; if (!el || !el.isConnected) return 'stale';
              el.scrollIntoView({{ block: 'center' }});
              if (el.tagName === 'SELECT') {{
                const option = Array.from(el.options).find((o) => o.text.trim().toLowerCase() === {wanted} || o.value.toLowerCase() === {wanted});
                if (!option) return 'no option';
                el.value = option.value;
                el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                return 'selected';
              }}
              el.focus();
              if ({clear}) {{
                if (el.isContentEditable) {{
                  const range = document.createRange(); range.selectNodeContents(el);
                  const selection = getSelection(); selection.removeAllRanges(); selection.addRange(range);
                }} else if (typeof el.select === 'function') {{ try {{ el.select(); }} catch (_) {{}} }}
              }}
              return el === document.activeElement || el.contains(document.activeElement) ? 'focused' : 'unfocused'; }})()",
            slot(index)?
        ))?;
        match outcome.as_str() {
            Some("focused") => {
                // Like typing: the page sees input events, and any script can be entered.
                page.call("Input.insertText", json!({ "text": text }))?;
            }
            Some("selected") => {}
            Some("no option") => return Err(format!("Dropdown {index} has no option \"{text}\".")),
            Some("unfocused") => return Err(format!("Element {index} does not take text; pick a field from the snapshot.")),
            _ => return Err(stale(index)),
        }
        if submit {
            for kind in ["keyDown", "keyUp"] {
                let mut key = json!({ "type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13 });
                if kind == "keyDown" {
                    key["text"] = "\r".into();
                }
                page.call("Input.dispatchKeyEvent", key)?;
            }
        }
        page.settle()?;
        page.snapshot()
    })
}

/// A JPEG screenshot of the visible part of `owner`'s page, with a line
/// saying what it shows.
///
/// # Errors
/// The browser failed.
pub fn screenshot(owner: u64, cancel: &AtomicBool) -> Result<(String, Vec<u8>), String> {
    with_page(owner, cancel, |page| {
        let shot = page.call("Page.captureScreenshot", json!({ "format": "jpeg", "quality": 80 }))?;
        let jpeg = shot.get("data").and_then(Value::as_str).and_then(base64_decode).ok_or("The browser sent no screenshot.")?;
        let url = page.evaluate("location.href")?;
        Ok((format!("Took a screenshot of {}; it follows as an image.", url.as_str().unwrap_or("the page")), jpeg))
    })
}

/// Closes conversation `owner`'s tab (when it is deleted); quits the
/// browser if no other conversation has one.
pub fn close(owner: u64) {
    let mut guard = browser();
    let Some(browser) = guard.as_mut() else { return };
    let Some(index) = browser.tabs.iter().position(|tab| tab.owner == owner) else { return };
    let tab = browser.tabs.remove(index);
    if browser.tabs.is_empty() {
        *guard = None;
    } else {
        let _ = browser.call(None, "Target.closeTarget", json!({ "targetId": tab.target }), &AtomicBool::new(false));
    }
}

/// Quits the browser; called when the app exits.
pub fn shutdown() {
    drop(browser().take());
}

/// Picks the browser to use, from Settings (`key`; `None` for Auto).
pub fn set_choice(key: Option<String>) {
    *CHOICE.lock().unwrap_or_else(PoisonError::into_inner) = key;
}

fn choice() -> Option<String> {
    CHOICE.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The name of the browser stored as `key`, if it is a supported one.
#[must_use]
pub fn name(key: &str) -> Option<&'static str> {
    KINDS.iter().find(|kind| kind.key == key).map(|kind| kind.name)
}

/// The browser to start: the one picked, or the first found for Auto or
/// when the one picked is not installed.
fn chosen(found: Vec<Installed>, key: Option<&str>) -> Option<Installed> {
    let picked = found.iter().position(|browser| Some(browser.key) == key).unwrap_or(0);
    found.into_iter().nth(picked)
}

/// The supported browsers installed, in the order Auto prefers them. Reads
/// the disk, so never on the UI thread.
#[must_use]
pub fn installed() -> Vec<Installed> {
    let roots: Vec<PathBuf> = if cfg!(target_os = "windows") {
        ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"].into_iter().filter_map(std::env::var_os).map(PathBuf::from).collect()
    } else if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Applications"));
        [Some(PathBuf::from("/Applications")), home].into_iter().flatten().collect()
    } else {
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default();
        // Snap's Chromium can't read a profile in /tmp, so it comes last.
        dirs.retain(|dir| !dir.starts_with("/snap"));
        dirs.push(PathBuf::from("/snap/bin"));
        dirs
    };
    KINDS
        .iter()
        .filter_map(|kind| {
            let apps: &[&str] = if cfg!(target_os = "windows") {
                std::slice::from_ref(&kind.windows)
            } else if cfg!(target_os = "macos") {
                std::slice::from_ref(&kind.macos)
            } else {
                kind.linux
            };
            let path = apps.iter().flat_map(|app| roots.iter().map(move |root| root.join(app))).find(|path| path.is_file())?;
            Some(Installed { key: kind.key, name: kind.name, path })
        })
        .collect()
}

/// The port and WebSocket path from a `DevToolsActivePort` file, once it
/// has been written completely.
fn parse_active_port(text: &str) -> Option<(u16, String)> {
    let mut lines = text.lines();
    let port = lines.next()?.trim().parse::<u16>().ok().filter(|port| *port != 0)?;
    let path = lines.next()?.trim();
    path.starts_with("/devtools/browser/").then(|| (port, path.to_owned()))
}

/// Decodes standard base64 (padding optional); `None` on any other character.
pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut bits, mut count) = (0u32, 0u32);
    for &c in text.trim_end_matches('=').as_bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_choice_or_first() {
        let found = || ["brave", "edge"].map(|key| Installed { key, name: "", path: PathBuf::from(key) }).to_vec();
        assert_eq!(chosen(found(), Some("edge")).map(|b| b.key), Some("edge"));
        assert_eq!(chosen(found(), None).map(|b| b.key), Some("brave"));
        assert_eq!(chosen(found(), Some("chrome")).map(|b| b.key), Some("brave"), "not installed: Auto");
        assert_eq!(chosen(Vec::new(), Some("edge")), None);
        assert!(KINDS.iter().position(|k| k.key == "helium") < KINDS.iter().position(|k| k.key == "edge"));
        assert!(KINDS.iter().position(|k| k.key == "brave") < KINDS.iter().position(|k| k.key == "edge"));
    }

    #[test]
    fn devtools_port_file() {
        assert_eq!(parse_active_port("9222\n/devtools/browser/abc\n"), Some((9222, "/devtools/browser/abc".into())));
        assert_eq!(parse_active_port("9222\n"), None, "half written");
        assert_eq!(parse_active_port("0\n/devtools/browser/abc"), None);
        assert_eq!(parse_active_port("x\n/devtools/browser/abc"), None);
        assert_eq!(parse_active_port("9222\n/elsewhere"), None);
    }

    #[test]
    fn base64() {
        for bytes in [&b""[..], b"f", b"fo", b"foo", b"foob", &[0xFF, 0xFE, 0xFD, 0x00]] {
            let encoded = serechat::data_url("x", bytes);
            let (_, text) = encoded.split_once(',').unwrap();
            assert_eq!(base64_decode(text).as_deref(), Some(bytes), "{text}");
        }
        assert_eq!(base64_decode("Zm9v\n"), None);
    }

    #[test]
    fn closing_without_a_browser_is_harmless() {
        close(u64::MAX);
        shutdown();
        assert!(browser().is_none());
        let stopped = AtomicBool::new(true);
        assert!(open(u64::MAX, "file:///etc/passwd", &stopped).unwrap_err().contains("http"));
        assert!(open(u64::MAX, "javascript:alert(1)", &stopped).is_err());
    }
}
