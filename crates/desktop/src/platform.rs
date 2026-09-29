//! Operating-system integration through the tools each platform ships with:
//! opening URLs and folders, and native file/folder pickers. No shell is
//! involved: every argument is passed to the program directly.
//!
//! The pickers block until the user answers, so call them on a worker thread.
//!
//! ponytail: pickers run a helper process (PowerShell / `osascript` /
//! `zenity`/`kdialog`) instead of linking the platform dialog APIs; they take
//! a moment to appear. Swap in direct Win32/Cocoa/portal calls if that grates.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Configures `command` to open no console window (Windows only; a GUI app
/// spawning console programs would otherwise flash one).
pub fn no_window(command: &mut Command) -> &mut Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Starts `command` detached from our stdio and reaps it in the background.
fn launch(mut command: Command) -> io::Result<()> {
    let mut child = no_window(&mut command).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Opens `url` in the default browser.
///
/// # Errors
/// The launcher could not be started.
pub fn open_url(url: &str) -> io::Result<()> {
    let command = if cfg!(target_os = "windows") {
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    launch(command)
}

/// Shows `path` in the file manager.
///
/// # Errors
/// The file manager could not be started.
pub fn open_folder(path: &Path) -> io::Result<()> {
    let program = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut command = Command::new(program);
    command.arg(path);
    launch(command)
}

/// Runs a picker helper and returns the non-empty lines it printed.
fn run_picker(mut command: Command) -> io::Result<Vec<String>> {
    let output = no_window(&mut command).stdin(Stdio::null()).stderr(Stdio::null()).output()?;
    // A cancelled dialog exits non-zero or prints nothing; both mean "none".
    Ok(String::from_utf8_lossy(&output.stdout).lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect())
}

/// PowerShell prelude: an invisible topmost owner so the dialog appears in
/// front of our window, and UTF-8 output.
const PS_PRELUDE: &str = "Add-Type -AssemblyName System.Windows.Forms; \
    [Console]::OutputEncoding = [Text.Encoding]::UTF8; \
    $owner = New-Object System.Windows.Forms.Form -Property @{TopMost = $true; ShowInTaskbar = $false; Opacity = 0}; \
    $owner.Show(); $owner.Activate();";

fn powershell(script: &str) -> io::Result<Vec<String>> {
    // PowerShell 7 shows the modern folder dialog; Windows PowerShell 5 is
    // always present as a fallback.
    let run = |program: &str| {
        let mut command = Command::new(program);
        command.args(["-NoProfile", "-NonInteractive", "-STA", "-Command", &format!("{PS_PRELUDE} {script}")]);
        run_picker(command)
    };
    run("pwsh").or_else(|_| run("powershell"))
}

/// Tries each Linux dialog helper in turn.
fn linux_picker(zenity: &[&str], kdialog: &[&str]) -> io::Result<Vec<String>> {
    let mut command = Command::new("zenity");
    command.args(zenity);
    run_picker(command).or_else(|_| {
        let mut command = Command::new("kdialog");
        command.args(kdialog);
        run_picker(command)
    })
}

/// Asks the user for files to attach. Empty when cancelled.
///
/// # Errors
/// No dialog helper could be started.
pub fn pick_files() -> io::Result<Vec<PathBuf>> {
    let lines = if cfg!(target_os = "windows") {
        powershell(
            "$d = New-Object System.Windows.Forms.OpenFileDialog -Property @{Multiselect = $true; Title = 'Attach files'}; \
             if ($d.ShowDialog($owner) -eq 'OK') { $d.FileNames -join \"`n\" }",
        )?
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("osascript");
        command.args([
            "-e",
            "set fs to choose file with prompt \"Attach files\" with multiple selections allowed",
            "-e",
            "set out to \"\"",
            "-e",
            "repeat with f in fs",
            "-e",
            "set out to out & POSIX path of f & linefeed",
            "-e",
            "end repeat",
            "-e",
            "return out",
        ]);
        run_picker(command)?
    } else {
        linux_picker(
            &["--file-selection", "--multiple", "--separator=\n", "--title=Attach files"],
            &["--getopenfilename", ".", "--multiple", "--separate-output", "--title", "Attach files"],
        )?
    };
    Ok(lines.into_iter().map(PathBuf::from).collect())
}

/// Asks the user for a project folder. `None` when cancelled.
///
/// # Errors
/// No dialog helper could be started.
pub fn pick_folder() -> io::Result<Option<PathBuf>> {
    let lines = if cfg!(target_os = "windows") {
        powershell(
            "$d = New-Object System.Windows.Forms.FolderBrowserDialog; $d.Description = 'Open a project folder'; \
             if ($d | Get-Member UseDescriptionForTitle) { $d.UseDescriptionForTitle = $true }; \
             if ($d.ShowDialog($owner) -eq 'OK') { $d.SelectedPath }",
        )?
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("osascript");
        command.args(["-e", "POSIX path of (choose folder with prompt \"Open a project folder\")"]);
        run_picker(command)?
    } else {
        linux_picker(&["--file-selection", "--directory", "--title=Open a project folder"], &["--getexistingdirectory", "."])?
    };
    Ok(lines.into_iter().next().map(PathBuf::from).filter(|p| p.is_dir()))
}
