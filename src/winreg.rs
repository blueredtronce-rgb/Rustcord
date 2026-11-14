//! HKCU registry values through windows-sys (no reg.exe): the sign-in autostart entry (Windows
//! Settings page, installer) and the installer's Apps & features entry. The installer
//! (`src/bin/rustcord-setup`) includes this file with `#[path]`.
//! OWNERSHIP: desktop engineer.

use std::path::Path;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_DWORD, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW, RegSetKeyValueW,
};

/// `HKCU\` + this: what Windows starts at sign-in, one value per program.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The Run value's command line: the quoted exe, plus `--minimized` to start in the tray.
pub fn run_command(exe: &Path, minimized: bool) -> String {
    format!("\"{}\"{}", exe.display(), if minimized { " --minimized" } else { "" })
}

/// The sign-in entry `name`'s command line, if there is one.
#[allow(dead_code)] // the app's
pub fn autostart(name: &str) -> Option<String> {
    get_string(RUN_KEY, name)
}

/// Writes the sign-in entry `name` for `exe`, or removes it (`None`).
pub fn set_autostart(name: &str, exe: Option<&Path>, minimized: bool) -> Result<(), String> {
    match exe {
        Some(exe) => set_string(RUN_KEY, name, &run_command(exe, minimized)),
        None => delete_value(RUN_KEY, name),
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn check(code: WIN32_ERROR, missing_ok: bool) -> Result<(), String> {
    match code {
        ERROR_SUCCESS => Ok(()),
        ERROR_FILE_NOT_FOUND if missing_ok => Ok(()),
        e => Err(std::io::Error::from_raw_os_error(e as i32).to_string()),
    }
}

/// Sets a REG_SZ value; the key is created if needed.
pub fn set_string(key: &str, name: &str, value: &str) -> Result<(), String> {
    let data = wide(value);
    // SAFETY: NUL-terminated UTF-16 strings; the size is in bytes, terminator included.
    check(unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(name).as_ptr(), REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) }, false)
}

/// Sets a REG_DWORD value; the key is created if needed.
#[allow(dead_code)] // the installer's
pub fn set_dword(key: &str, name: &str, value: u32) -> Result<(), String> {
    // SAFETY: as in `set_string`, with a 4-byte value.
    check(unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(name).as_ptr(), REG_DWORD, (&raw const value).cast(), 4) }, false)
}

/// A REG_SZ value, None when missing (or not a string).
pub fn get_string(key: &str, name: &str) -> Option<String> {
    let (key, name) = (wide(key), wide(name));
    let mut buf = vec![0u16; 260];
    loop {
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `bytes` bytes; RegGetValueW NUL-terminates REG_SZ data.
        let code = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut bytes)
        };
        match code {
            ERROR_SUCCESS => {
                let units = &buf[..bytes as usize / 2];
                return String::from_utf16(units.strip_suffix(&[0]).unwrap_or(units)).ok();
            }
            // ERROR_MORE_DATA: `bytes` is the size needed.
            234 if (bytes as usize) / 2 > buf.len() => buf.resize(bytes as usize / 2 + 1, 0),
            _ => return None,
        }
    }
}

/// Removes a value; one that is not there is fine.
pub fn delete_value(key: &str, name: &str) -> Result<(), String> {
    // SAFETY: NUL-terminated UTF-16 strings.
    check(unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(name).as_ptr()) }, true)
}

/// Removes a key with everything under it; one that is not there is fine.
#[allow(dead_code)] // the installer's
pub fn delete_key(key: &str) -> Result<(), String> {
    // SAFETY: NUL-terminated UTF-16 string; a non-empty subkey, so the key itself goes too.
    check(unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(key).as_ptr()) }, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_value_quotes_the_exe() {
        let exe = Path::new(r"C:\Users\Ann Lee\AppData\Local\Programs\Rustcord\rustcord.exe");
        assert_eq!(run_command(exe, true), r#""C:\Users\Ann Lee\AppData\Local\Programs\Rustcord\rustcord.exe" --minimized"#);
        assert_eq!(run_command(exe, false), r#""C:\Users\Ann Lee\AppData\Local\Programs\Rustcord\rustcord.exe""#);
    }

    #[test]
    fn values_round_trip_and_leave_nothing() {
        // A throwaway key of its own, never the real Run or Uninstall entries.
        let key = format!(r"Software\RustcordSetupTest-unit-{}", std::process::id());
        let long = "x".repeat(700);
        set_string(&key, "Name", "Rustcord ü").unwrap();
        set_string(&key, "Long", &long).unwrap();
        set_dword(&key, "NoModify", 1).unwrap();
        assert_eq!(get_string(&key, "Name").as_deref(), Some("Rustcord ü"));
        assert_eq!(get_string(&key, "Long"), Some(long));
        assert_eq!(get_string(&key, "NoModify"), None, "a DWORD is not a string");
        delete_value(&key, "Name").unwrap();
        delete_value(&key, "Name").unwrap();
        assert_eq!(get_string(&key, "Name"), None);
        delete_key(&key).unwrap();
        delete_key(&key).unwrap();
        assert_eq!(get_string(&key, "Long"), None);
    }
}
