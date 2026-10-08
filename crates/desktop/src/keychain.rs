//! The sign-in's refresh token, kept in the operating system's credential
//! store: Credential Manager on Windows, the login keychain on macOS
//! (through `security`) and the Secret Service on Linux (through
//! `secret-tool`).
//!
//! Writes go through the app's `Writer` thread, in order; reads may run
//! anywhere off the UI thread.
//!
//! ponytail: Linux without `secret-tool` or a running Secret Service keeps
//! the token in `~/.serechat/serechat-desktop.token` (mode `0600`); speak the
//! Secret Service's D-Bus protocol directly if that is not good enough.

/// The entry's name. Tests use their own, so they never touch a real sign-in.
const NAME: &str = if cfg!(test) { "serechat-desktop-test" } else { "serechat-desktop" };

pub use imp::{delete, load, save};

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::io;
    use std::ptr;

    /// `CREDENTIALW`.
    #[repr(C)]
    #[allow(dead_code, reason = "Windows reads the fields")]
    struct Credential {
        flags: u32,
        kind: u32,
        target_name: *mut u16,
        comment: *mut u16,
        /// `FILETIME`.
        last_written: [u32; 2],
        blob_size: u32,
        blob: *mut u8,
        persist: u32,
        attribute_count: u32,
        attributes: *mut c_void,
        target_alias: *mut u16,
        user_name: *mut u16,
    }

    const CRED_TYPE_GENERIC: u32 = 1;
    /// Kept for this user on this machine; never roams.
    const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;
    const ERROR_NOT_FOUND: i32 = 1168;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn CredReadW(target: *const u16, kind: u32, flags: u32, credential: *mut *mut Credential) -> i32;
        fn CredWriteW(credential: *const Credential, flags: u32) -> i32;
        fn CredDeleteW(target: *const u16, kind: u32, flags: u32) -> i32;
        fn CredFree(buffer: *mut c_void);
    }

    /// `text` as a NUL-terminated UTF-16 string.
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    /// The stored refresh token, if any.
    #[must_use]
    pub fn load() -> Option<String> {
        let target = wide(super::NAME);
        let mut credential = ptr::null_mut();
        // SAFETY: `target` is NUL-terminated and `credential` a valid out pointer.
        if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &raw mut credential) } == 0 {
            return None;
        }
        // SAFETY: the call succeeded, so `credential` points to a credential
        // whose blob holds `blob_size` bytes; it is freed once, after the copy.
        unsafe {
            let blob = (*credential).blob;
            let bytes = if blob.is_null() { Vec::new() } else { std::slice::from_raw_parts(blob, (*credential).blob_size as usize).to_vec() };
            CredFree(credential.cast());
            String::from_utf8(bytes).ok().filter(|t| !t.is_empty())
        }
    }

    /// Stores `token`, replacing the one before.
    ///
    /// # Errors
    /// Credential Manager refused it.
    pub fn save(token: &str) -> io::Result<()> {
        let mut target = wide(super::NAME);
        let mut user = wide("SereChat");
        let mut blob = token.as_bytes().to_vec();
        let credential = Credential {
            flags: 0,
            kind: CRED_TYPE_GENERIC,
            target_name: target.as_mut_ptr(),
            comment: ptr::null_mut(),
            last_written: [0; 2],
            blob_size: u32::try_from(blob.len()).map_err(io::Error::other)?,
            blob: blob.as_mut_ptr(),
            persist: CRED_PERSIST_LOCAL_MACHINE,
            attribute_count: 0,
            attributes: ptr::null_mut(),
            target_alias: ptr::null_mut(),
            user_name: user.as_mut_ptr(),
        };
        // SAFETY: every pointer refers to a buffer that outlives the call.
        if unsafe { CredWriteW(&raw const credential, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Removes the stored token; nothing stored is not an error.
    ///
    /// # Errors
    /// Credential Manager refused it.
    pub fn delete() -> io::Result<()> {
        let target = wide(super::NAME);
        // SAFETY: `target` is NUL-terminated.
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_NOT_FOUND) {
                return Err(error);
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::io::{self, Write};
    use std::process::{Command, Stdio};

    const SECURITY: &str = "/usr/bin/security";
    const ACCOUNT: &str = "refresh-token";

    /// The stored refresh token, if any.
    #[must_use]
    pub fn load() -> Option<String> {
        let output = Command::new(SECURITY)
            .args(["find-generic-password", "-s", super::NAME, "-a", ACCOUNT, "-w"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        String::from_utf8(output.stdout).ok().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
    }

    /// Stores `token`, replacing the one before.
    ///
    /// # Errors
    /// `security` could not be run, or the keychain did not take the token.
    pub fn save(token: &str) -> io::Result<()> {
        // Written to `security -i` rather than passed as an argument, so the
        // token never shows in the process list. Tokens hold no spaces or
        // quotes (the API client checks).
        let mut child = Command::new(SECURITY).arg("-i").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            writeln!(stdin, "add-generic-password -U -s {} -a {ACCOUNT} -w {token}", super::NAME)?;
        }
        child.wait()?;
        // Interactive mode's exit status says little; reading back does.
        if load().as_deref() == Some(token) { Ok(()) } else { Err(io::Error::other("the keychain did not take the sign-in")) }
    }

    /// Removes the stored token; nothing stored is not an error.
    ///
    /// # Errors
    /// `security` could not be run or failed.
    pub fn delete() -> io::Result<()> {
        let status = Command::new(SECURITY)
            .args(["delete-generic-password", "-s", super::NAME, "-a", ACCOUNT])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        // 44: the item was not found.
        if status.success() || status.code() == Some(44) { Ok(()) } else { Err(io::Error::other(format!("security failed ({status})"))) }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    const ATTRIBUTES: [&str; 4] = ["service", super::NAME, "account", "refresh-token"];

    /// Where the token goes without a Secret Service.
    fn fallback() -> io::Result<PathBuf> {
        serechat::Config::dir().map(|dir| dir.join(format!("{}.token", super::NAME))).map_err(io::Error::other)
    }

    /// The stored refresh token, if any.
    #[must_use]
    pub fn load() -> Option<String> {
        let output = Command::new("secret-tool").arg("lookup").args(ATTRIBUTES).stdin(Stdio::null()).stderr(Stdio::null()).output();
        let stored = output.ok().filter(|o| o.status.success()).and_then(|o| String::from_utf8(o.stdout).ok());
        let token = stored.or_else(|| std::fs::read_to_string(fallback().ok()?).ok());
        token.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
    }

    /// Stores `token`, replacing the one before.
    ///
    /// # Errors
    /// Neither the Secret Service nor the fallback file took it.
    pub fn save(token: &str) -> io::Result<()> {
        let stored = Command::new("secret-tool")
            .args(["store", "--label=SereChat Desktop"])
            .args(ATTRIBUTES)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .and_then(|mut child| {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(token.as_bytes())?;
                }
                child.wait()
            });
        if stored.is_ok_and(|status| status.success()) {
            // An older token in the fallback file must not outlive this one.
            return remove(fallback()?);
        }
        serechat::write_private(&fallback()?, token.as_bytes()).map_err(io::Error::other)
    }

    /// Removes the stored token; nothing stored is not an error.
    ///
    /// # Errors
    /// The fallback file could not be removed.
    pub fn delete() -> io::Result<()> {
        // No `secret-tool`, or nothing stored: either way nothing is left there.
        let _ = Command::new("secret-tool").arg("clear").args(ATTRIBUTES).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        remove(fallback()?)
    }

    fn remove(path: PathBuf) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against the real credential store, under a test-only name:
    /// `cargo test -p serechat-desktop live_keychain -- --ignored`.
    #[test]
    #[ignore = "touches the operating system's credential store"]
    fn live_keychain() {
        delete().unwrap();
        assert_eq!(load(), None);
        save("sc_refresh_abc-_1").unwrap();
        assert_eq!(load().as_deref(), Some("sc_refresh_abc-_1"));
        save("sc_refresh_new").unwrap();
        assert_eq!(load().as_deref(), Some("sc_refresh_new"), "a save replaces the token");
        delete().unwrap();
        delete().unwrap();
        assert_eq!(load(), None);
    }
}
