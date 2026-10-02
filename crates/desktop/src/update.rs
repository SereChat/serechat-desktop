//! Updates from the project's GitHub releases.
//!
//! A background thread checks the latest release shortly after startup and
//! every six hours. When it is newer and installing is on, it downloads the
//! release archive for this platform, checks its size and the SHA-256
//! digest GitHub publishes, unpacks it with the `tar` every supported OS
//! ships (Windows' bsdtar reads zip files), and swaps it in next to the
//! running copy:
//!
//! - Windows: the running `serechat.exe` is renamed aside (Windows allows
//!   that, not overwriting it) and removed on the next start.
//! - Linux: the new binary is renamed over the old one.
//! - macOS: the whole `SereChat.app` bundle is replaced.
//!
//! The new version runs from the next start; "Restart" starts it at once.
//! A copy that can't write to its own folder (installed system-wide) only
//! says an update is available. Development builds never update.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use serde_json::Value;
use serechat::Client;

use crate::sha256::{Sha256, hex};

/// Where releases are published.
const LATEST: &str = "https://api.github.com/repos/SereChat/serechat-desktop/releases/latest";
/// Time between checks.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// Wait after startup before the first check, so it never competes with it.
const FIRST_CHECK: Duration = Duration::from_secs(8);
/// Largest archive accepted.
const MAX_ARCHIVE: u64 = 512 << 20;

/// What the updater is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Updates are off, and why.
    Disabled(String),
    /// Not checked yet.
    Idle,
    /// Asking GitHub.
    Checking,
    /// This is the latest version.
    UpToDate,
    /// A newer version exists and was not installed (installing is off, or
    /// this copy can't install it); where to download it by hand.
    Available {
        /// The new version.
        version: String,
        /// Its release page.
        url: String,
        /// Why it was not installed, if it was tried.
        note: Option<String>,
    },
    /// Downloading and installing a version.
    Installing(String),
    /// A version is installed and runs after a restart.
    Ready(String),
    /// Checking failed.
    Failed(String),
}

/// What the user asked for.
enum Request {
    Check,
    Install,
}

/// The update thread's controls.
pub struct Updater {
    requests: Option<mpsc::Sender<Request>>,
    auto: Arc<AtomicBool>,
}

/// A release newer than this copy.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Release {
    version: String,
    page: String,
    /// This platform's archive: download URL, size and SHA-256 (hex), if
    /// the release has one.
    asset: Option<(String, u64, Option<String>)>,
}

impl Updater {
    /// Starts the update thread; `report` hears every change of status.
    /// With `auto`, new versions install themselves.
    pub fn start(client: Client, auto: bool, report: impl Fn(Status) + Send + 'static) -> Self {
        let auto = Arc::new(AtomicBool::new(auto));
        if let Some(why) = disabled() {
            report(Status::Disabled(why));
            return Self { requests: None, auto };
        }
        let (requests, inbox) = mpsc::channel();
        let flag = Arc::clone(&auto);
        let spawned = std::thread::Builder::new().name("serechat-update".into()).spawn(move || run(&client, &flag, &inbox, &report));
        if let Err(e) = spawned {
            eprintln!("serechat: cannot start the update thread: {e}");
        }
        Self { requests: Some(requests), auto }
    }

    /// Checks now.
    pub fn check(&self) {
        if let Some(requests) = &self.requests {
            let _ = requests.send(Request::Check);
        }
    }

    /// Installs the available version now.
    pub fn install(&self) {
        if let Some(requests) = &self.requests {
            let _ = requests.send(Request::Install);
        }
    }

    /// Turns automatic installing on or off.
    pub fn set_auto(&self, auto: bool) {
        self.auto.store(auto, Ordering::Relaxed);
    }
}

/// Why this copy never updates itself, if it doesn't.
fn disabled() -> Option<String> {
    if cfg!(debug_assertions) {
        return Some("Development builds don't update.".into());
    }
    if asset_name().is_none() {
        return Some("There are no release builds for this platform.".into());
    }
    let exe = std::env::current_exe().ok()?;
    let folder = exe.parent()?;
    let in_target = folder.file_name().is_some_and(|n| n == "release" || n == "debug") && folder.parent().and_then(Path::file_name).is_some_and(|n| n == "target");
    in_target.then(|| "This copy runs from a build folder; it updates from source.".into())
}

/// The update thread: checks on a timer or when asked, installs when it
/// should.
fn run(client: &Client, auto: &AtomicBool, inbox: &mpsc::Receiver<Request>, report: &dyn Fn(Status)) {
    cleanup();
    let mut wait = FIRST_CHECK;
    let mut latest: Option<Release> = None;
    let mut installed: Option<String> = None;
    loop {
        let request = match inbox.recv_timeout(wait) {
            Ok(request) => request,
            Err(mpsc::RecvTimeoutError::Timeout) => Request::Check,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        wait = CHECK_EVERY;
        if matches!(request, Request::Check) {
            report(Status::Checking);
            match check(client) {
                Ok(release) => latest = release,
                Err(e) => {
                    report(Status::Failed(e));
                    continue;
                }
            }
        }
        let Some(release) = latest.clone() else {
            report(Status::UpToDate);
            continue;
        };
        if installed.as_ref() == Some(&release.version) {
            report(Status::Ready(release.version));
            continue;
        }
        let wanted = matches!(request, Request::Install) || auto.load(Ordering::Relaxed);
        if !wanted || release.asset.is_none() {
            let note = release.asset.is_none().then(|| "This release has no build for this platform.".to_owned());
            report(Status::Available { version: release.version, url: release.page, note });
            continue;
        }
        report(Status::Installing(release.version.clone()));
        match install(client, &release) {
            Ok(()) => {
                installed = Some(release.version.clone());
                report(Status::Ready(release.version));
            }
            Err(note) => report(Status::Available { version: release.version, url: release.page, note: Some(note) }),
        }
    }
}

/// The name of this platform's release archive.
fn asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("serechat-windows-x86_64.zip"),
        ("macos", "aarch64") => Some("serechat-macos-arm64.tar.gz"),
        ("linux", "x86_64") => Some("serechat-linux-x86_64.tar.gz"),
        _ => None,
    }
}

/// The latest release, if it is newer than this copy.
///
/// # Errors
/// GitHub could not be reached or sent something unexpected.
fn check(client: &Client) -> Result<Option<Release>, String> {
    let (_, body) = client.fetch_text(LATEST, 1 << 20).map_err(|e| match e {
        serechat::Error::Api { status: 404, .. } => "No release has been published yet.".to_owned(),
        e => format!("Could not check for updates: {e}"),
    })?;
    parse_release(&body, env!("CARGO_PKG_VERSION"), asset_name().unwrap_or_default())
}

/// Reads GitHub's release JSON: the release if it is newer than `current`,
/// with the asset named `asset`.
fn parse_release(body: &str, current: &str, asset: &str) -> Result<Option<Release>, String> {
    let json: Value = serde_json::from_str(body).map_err(|_| "GitHub sent an unexpected answer.".to_owned())?;
    let tag = json.get("tag_name").and_then(Value::as_str).ok_or("GitHub sent a release without a version.")?;
    let version = tag.trim_start_matches('v').to_owned();
    if json.get("draft").and_then(Value::as_bool) == Some(true) || json.get("prerelease").and_then(Value::as_bool) == Some(true) || !newer(&version, current) {
        return Ok(None);
    }
    let page = json.get("html_url").and_then(Value::as_str).unwrap_or("https://github.com/SereChat/serechat-desktop/releases").to_owned();
    let asset = json.get("assets").and_then(Value::as_array).and_then(|assets| {
        let found = assets.iter().find(|a| a.get("name").and_then(Value::as_str) == Some(asset))?;
        let url = found.get("browser_download_url").and_then(Value::as_str)?.to_owned();
        let size = found.get("size").and_then(Value::as_u64).unwrap_or(0);
        let digest = found.get("digest").and_then(Value::as_str).and_then(|d| d.strip_prefix("sha256:")).map(str::to_ascii_lowercase);
        Some((url, size, digest))
    });
    Ok(Some(Release { version, page, asset }))
}

/// Whether version `a` (`1.2.3`) is newer than `b`. Missing parts count as 0;
/// anything after a `-` or `+` is ignored.
fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split(['-', '+']).next().unwrap_or_default().split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let (a, b) = (parts(a), parts(b));
    let len = a.len().max(b.len());
    let at = |v: &[u64], i: usize| v.get(i).copied().unwrap_or(0);
    (0..len).map(|i| at(&a, i).cmp(&at(&b, i))).find(|o| o.is_ne()) == Some(std::cmp::Ordering::Greater)
}

/// What gets replaced: the running executable, or on macOS its app bundle.
fn install_target() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().and_then(fs::canonicalize).map_err(|e| format!("This copy's location is unknown: {e}"))?;
    if cfg!(target_os = "macos") {
        // …/SereChat.app/Contents/MacOS/serechat
        if let Some(bundle) = exe.ancestors().nth(3).filter(|b| b.extension().is_some_and(|e| e == "app")) {
            return Ok(bundle.to_path_buf());
        }
    }
    Ok(exe)
}

/// Downloads, checks, unpacks and swaps in `release`.
///
/// # Errors
/// Why it could not, for the user.
fn install(client: &Client, release: &Release) -> Result<(), String> {
    let (url, size, digest) = release.asset.clone().ok_or("This release has no build for this platform.")?;
    let target = install_target()?;
    let folder = target.parent().ok_or("This copy's folder is unknown.")?.to_path_buf();
    // Unpacked next to the target, so the final renames stay on one disk.
    let staging = folder.join(format!(".serechat-update-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("SereChat can't update itself in {}: {e}", folder.display()))?;
    let result = (|| {
        let name = asset_name().unwrap_or("update");
        let archive = staging.join(name);
        let mut file = fs::File::create(&archive).map_err(|e| e.to_string())?;
        client.download(&url, MAX_ARCHIVE, &mut file).map_err(|e| format!("The download failed: {e}"))?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        verify(&archive, size, digest.as_deref())?;
        let unpacked = staging.join("new");
        fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
        unpack(&archive, &unpacked)?;
        let fresh = find_new(&unpacked, &target).ok_or("The download does not contain SereChat.")?;
        swap(&fresh, &target)
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

/// Checks a download's size and SHA-256 against the release's.
fn verify(archive: &Path, size: u64, digest: Option<&str>) -> Result<(), String> {
    let mut file = fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut hash = Sha256::default();
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        total += n as u64;
    }
    if size > 0 && total != size {
        return Err(format!("The download is {total} bytes; the release says {size}."));
    }
    if let Some(expected) = digest
        && hex(&hash.finish()) != expected
    {
        return Err("The download does not match the release's checksum.".into());
    }
    Ok(())
}

/// Unpacks `archive` into `into` with the system's `tar`.
fn unpack(archive: &Path, into: &Path) -> Result<(), String> {
    // On Windows, the bsdtar that ships in System32 (it reads zip); a GNU tar
    // from Git or MSYS earlier on PATH would not.
    let tar = std::env::var_os("SystemRoot").filter(|_| cfg!(windows)).map_or_else(|| PathBuf::from("tar"), |root| PathBuf::from(root).join("System32").join("tar.exe"));
    let mut command = Command::new(tar);
    command.arg("-xf").arg(archive).arg("-C").arg(into).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let output = crate::platform::no_window(&mut command).output().map_err(|e| format!("The download could not be unpacked: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("The download could not be unpacked: {}", String::from_utf8_lossy(&output.stderr).trim()))
    }
}

/// The new copy of `target` inside `unpacked`: a file or bundle of the
/// same name, at most two folders down.
fn find_new(unpacked: &Path, target: &Path) -> Option<PathBuf> {
    let name = target.file_name()?;
    let mut folders = vec![(unpacked.to_path_buf(), 0)];
    while let Some((folder, depth)) = folders.pop() {
        for entry in fs::read_dir(&folder).ok()?.flatten() {
            let path = entry.path();
            let kind = entry.file_type().ok()?;
            if path.file_name() == Some(name) && (kind.is_file() || (kind.is_dir() && target.is_dir())) {
                return Some(path);
            }
            if kind.is_dir() && depth < 2 {
                folders.push((path, depth + 1));
            }
        }
    }
    None
}

/// Puts `fresh` where `target` is, keeping `target` if anything fails.
fn swap(fresh: &Path, target: &Path) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("The new version could not be put in place: {e}");
    if target.is_dir() || cfg!(windows) {
        // Moved aside first: Windows can't overwrite a running executable,
        // and a bundle is a folder.
        let aside = aside_path(target);
        if aside.is_dir() {
            let _ = fs::remove_dir_all(&aside);
        } else {
            let _ = fs::remove_file(&aside);
        }
        fs::rename(target, &aside).map_err(fail)?;
        if let Err(e) = fs::rename(fresh, target) {
            let _ = fs::rename(&aside, target);
            return Err(fail(e));
        }
        // A bundle can go now (macOS keeps open files alive); Windows' exe
        // is still running and goes on the next start.
        if aside.is_dir() {
            let _ = fs::remove_dir_all(&aside);
        }
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(fresh, fs::Permissions::from_mode(0o755)).map_err(fail)?;
    }
    fs::rename(fresh, target).map_err(fail)
}

/// Where the previous copy goes while being replaced.
fn aside_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    target.with_file_name(name)
}

/// Removes what an earlier update left: the previous executable on
/// Windows, and unpacking folders of updates that were interrupted.
fn cleanup() {
    let Ok(target) = install_target() else { return };
    let _ = fs::remove_file(aside_path(&target));
    let _ = fs::remove_dir_all(aside_path(&target));
    if let Some(entries) = target.parent().and_then(|f| fs::read_dir(f).ok()) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(".serechat-update-") {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
}

/// Starts the installed (new) copy; called as the app exits for a restart.
pub fn relaunch() {
    let Ok(target) = install_target() else { return };
    let mut command = if target.is_dir() {
        let mut open = Command::new("open");
        open.arg("-n").arg(&target);
        open
    } else {
        Command::new(&target)
    };
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Err(e) = command.spawn() {
        eprintln!("serechat: cannot start the new version: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(newer("0.2.0", "0.1.0") && newer("1.0", "0.9.9") && newer("0.1.10", "0.1.9"));
        assert!(!newer("0.1.0", "0.1.0") && !newer("0.1", "0.1.0") && !newer("0.0.9", "0.1.0"));
        assert!(!newer("1.0.0-beta", "1.0.0") && newer("1.0.1-beta", "1.0.0"));
        assert!(!newer("garbage", "0.1.0"));
    }

    #[test]
    fn releases() {
        let body = r#"{ "tag_name": "v0.3.0", "html_url": "https://github.com/x/releases/tag/v0.3.0", "assets": [
            { "name": "serechat-linux-x86_64.tar.gz", "browser_download_url": "https://x/l.tgz", "size": 10, "digest": "sha256:ABC" },
            { "name": "serechat-windows-x86_64.zip", "browser_download_url": "https://x/w.zip", "size": 20 } ] }"#;
        let release = parse_release(body, "0.2.9", "serechat-linux-x86_64.tar.gz").unwrap().unwrap();
        assert_eq!(release.version, "0.3.0");
        assert_eq!(release.asset, Some(("https://x/l.tgz".into(), 10, Some("abc".into()))));
        let windows = parse_release(body, "0.2.9", "serechat-windows-x86_64.zip").unwrap().unwrap();
        assert_eq!(windows.asset, Some(("https://x/w.zip".into(), 20, None)));
        assert_eq!(parse_release(body, "0.3.0", "x").unwrap(), None, "not newer");
        assert!(parse_release(body, "0.1.0", "serechat-macos-arm64.tar.gz").unwrap().unwrap().asset.is_none());
        assert_eq!(parse_release(r#"{"tag_name": "v9.0.0", "prerelease": true}"#, "0.1.0", "x").unwrap(), None);
        assert!(parse_release("<html>", "0.1.0", "x").is_err());
    }

    #[test]
    fn downloads_are_checked_unpacked_and_swapped() {
        let dir = std::env::temp_dir().join(format!("serechat-update-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let source = dir.join("src");
        fs::create_dir_all(source.join("serechat-linux-x86_64")).unwrap();
        let binary = source.join("serechat-linux-x86_64").join("serechat");
        fs::write(&binary, b"new").unwrap();
        // Packed with the same tar the updater unpacks with.
        let archive = dir.join("a.tar.gz");
        let tar = std::env::var_os("SystemRoot").filter(|_| cfg!(windows)).map_or_else(|| PathBuf::from("tar"), |root| PathBuf::from(root).join("System32").join("tar.exe"));
        let packed = Command::new(tar).arg("-czf").arg(&archive).arg("-C").arg(&source).arg("serechat-linux-x86_64").status().unwrap();
        assert!(packed.success());

        let bytes = fs::read(&archive).unwrap();
        let digest = hex(&crate::sha256::digest(&bytes));
        assert!(verify(&archive, bytes.len() as u64, Some(&digest)).is_ok());
        assert!(verify(&archive, bytes.len() as u64 + 1, None).unwrap_err().contains("bytes"));
        assert!(verify(&archive, 0, Some("00")).unwrap_err().contains("checksum"));

        let unpacked = dir.join("new");
        fs::create_dir_all(&unpacked).unwrap();
        unpack(&archive, &unpacked).unwrap();
        let installed = dir.join("bin").join("serechat");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::write(&installed, b"old").unwrap();
        let fresh = find_new(&unpacked, &installed).unwrap();
        swap(&fresh, &installed).unwrap();
        assert_eq!(fs::read(&installed).unwrap(), b"new");
        assert!(find_new(&unpacked, &dir.join("missing")).is_none());
        fs::remove_dir_all(&dir).unwrap();
    }
}
