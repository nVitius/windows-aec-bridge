//! Per-user login startup registration and single-instance coordination.
//!
//! The login-startup functions intentionally target the executable returned by
//! [`std::env::current_exe`]. They should therefore be called by the GUI binary,
//! not by the companion CLI binary.

use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// Name of this application's value under the current user's `Run` key.
pub const RUN_VALUE_NAME: &str = "AEC Bridge";

/// Argument appended to the GUI path in the current user's `Run` key.
pub const LOGIN_STARTUP_ARGUMENT: &str = "--login-startup";

/// Session-local mutex name used to identify another running GUI instance.
pub const SINGLE_INSTANCE_MUTEX_NAME: &str =
    "Local\\AECBridge.SingleInstance.85b20345-16de-4d98-a7e5-c3ca5306d792";

/// State of the current user's login-startup registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginStartupStatus {
    /// The `AEC Bridge` value is absent.
    Disabled,
    /// The value exactly matches the current executable and expected argument.
    Current,
    /// A value exists, but points somewhere else or has different arguments.
    Stale { registered_command: String },
}

/// Outcome of trying to become the only AEC Bridge instance in this session.
#[derive(Debug)]
pub enum SingleInstance {
    /// This process holds the handle that marks this application instance active.
    Acquired(SingleInstanceGuard),
    /// Another process in this Windows session already has the mutex open.
    AlreadyRunning,
}

/// Keeps the session-local single-instance mutex alive until dropped.
pub struct SingleInstanceGuard {
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
    #[cfg(not(windows))]
    _private: (),
}

impl fmt::Debug for SingleInstanceGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SingleInstanceGuard")
            .finish_non_exhaustive()
    }
}

#[cfg(windows)]
impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;

        // SAFETY: `handle` is returned by `CreateMutexW`, is owned exclusively
        // by this guard, and is closed exactly once here.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// Return the exact command that should be stored for the current GUI binary.
pub fn login_startup_command() -> Result<String> {
    let executable = std::env::current_exe().context("failed to locate the current executable")?;
    command_for_executable(&executable)
}

/// Query whether login startup is disabled, current, or stale for this binary.
pub fn query_login_startup() -> Result<LoginStartupStatus> {
    let expected = login_startup_command()?;
    let registered = platform::read_login_startup_value()?;
    Ok(classify_login_startup(registered.as_deref(), &expected))
}

/// Create or update the current user's login-startup registration.
pub fn enable_login_startup() -> Result<()> {
    let command = login_startup_command()?;
    platform::write_login_startup_value(&command)
}

/// Remove the current user's login-startup registration.
///
/// Returns `true` when a value was removed and `false` when it was already
/// absent. Both outcomes are successful.
pub fn disable_login_startup() -> Result<bool> {
    platform::delete_login_startup_value()
}

/// Try to acquire the per-session AEC Bridge single-instance mutex.
///
/// The returned guard must remain alive for as long as the GUI is running.
pub fn acquire_single_instance() -> Result<SingleInstance> {
    platform::acquire_single_instance()
}

/// Show an informational dialog explaining that AEC Bridge is already running.
pub fn show_already_running_message() -> Result<()> {
    platform::show_already_running_message()
}

/// Show a native fatal-startup dialog for the console-free GUI binary.
pub fn show_startup_error_message(message: &str) -> Result<()> {
    platform::show_startup_error_message(message)
}

fn command_for_executable(executable: &Path) -> Result<String> {
    let executable = executable
        .to_str()
        .context("the executable path is not valid Unicode")?;
    if executable.is_empty() {
        bail!("the executable path is empty");
    }
    if executable.contains('"') {
        bail!("the executable path contains an unsupported double quote");
    }
    if executable.contains('\0') {
        bail!("the executable path contains an unsupported NUL character");
    }

    let command = format!("\"{executable}\" {LOGIN_STARTUP_ARGUMENT}");
    if command.encode_utf16().count() > 260 {
        bail!("the Windows startup command exceeds the 260-character limit");
    }
    Ok(command)
}

fn classify_login_startup(
    registered_command: Option<&str>,
    expected_command: &str,
) -> LoginStartupStatus {
    match registered_command {
        None => LoginStartupStatus::Disabled,
        Some(command) if command == expected_command => LoginStartupStatus::Current,
        Some(command) => LoginStartupStatus::Stale {
            registered_command: command.to_owned(),
        },
    }
}

#[cfg(windows)]
mod platform {
    use std::iter;

    use anyhow::{Context, Result, bail};
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA,
        ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, GetLastError, SetLastError,
    };
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
        REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW,
        RegQueryValueExW, RegSetValueExW,
    };
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW,
    };
    use windows::core::{PCWSTR, w};

    use super::{SINGLE_INSTANCE_MUTEX_NAME, SingleInstance, SingleInstanceGuard};

    const VALUE_QUERY_RETRIES: usize = 3;

    struct RegistryKey(HKEY);

    impl Drop for RegistryKey {
        fn drop(&mut self) {
            // SAFETY: the key is an owned handle returned by RegOpenKeyExW or
            // RegCreateKeyExW and is closed exactly once here.
            let _ = unsafe { RegCloseKey(self.0) };
        }
    }

    pub(super) fn read_login_startup_value() -> Result<Option<String>> {
        let Some(key) = open_run_key(KEY_QUERY_VALUE)? else {
            return Ok(None);
        };

        for _ in 0..VALUE_QUERY_RETRIES {
            let mut value_type = REG_VALUE_TYPE(0);
            let mut byte_count = 0u32;
            // SAFETY: all pointers refer to live local variables; no data
            // buffer is supplied during this size query.
            let status = unsafe {
                RegQueryValueExW(
                    key.0,
                    w!("AEC Bridge"),
                    None,
                    Some(&mut value_type),
                    None,
                    Some(&mut byte_count),
                )
            };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            status
                .ok()
                .context("failed to query the AEC Bridge login-startup value")?;
            if value_type != REG_SZ {
                bail!(
                    "AEC Bridge login-startup value has registry type {}, expected REG_SZ",
                    value_type.0
                );
            }

            let mut bytes = vec![0u8; byte_count as usize];
            let mut actual_byte_count = byte_count;
            // SAFETY: the buffer is writable for `actual_byte_count` bytes and
            // remains alive for the duration of the call.
            let status = unsafe {
                RegQueryValueExW(
                    key.0,
                    w!("AEC Bridge"),
                    None,
                    Some(&mut value_type),
                    if bytes.is_empty() {
                        None
                    } else {
                        Some(bytes.as_mut_ptr())
                    },
                    Some(&mut actual_byte_count),
                )
            };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            if status == ERROR_MORE_DATA {
                continue;
            }
            status
                .ok()
                .context("failed to read the AEC Bridge login-startup value")?;
            if value_type != REG_SZ {
                bail!(
                    "AEC Bridge login-startup value changed to registry type {} while reading",
                    value_type.0
                );
            }
            bytes.truncate(actual_byte_count as usize);
            return decode_registry_string(&bytes).map(Some);
        }

        bail!("AEC Bridge login-startup value changed repeatedly while it was being read")
    }

    pub(super) fn write_login_startup_value(command: &str) -> Result<()> {
        let key = create_run_key()?;
        let bytes = encode_registry_string(command);
        // SAFETY: the registry handle is valid and `bytes` contains a complete
        // NUL-terminated UTF-16 REG_SZ payload for the duration of the call.
        unsafe { RegSetValueExW(key.0, w!("AEC Bridge"), None, REG_SZ, Some(&bytes)) }
            .ok()
            .context("failed to enable AEC Bridge at login")
    }

    pub(super) fn delete_login_startup_value() -> Result<bool> {
        let Some(key) = open_run_key(KEY_SET_VALUE)? else {
            return Ok(false);
        };
        // SAFETY: `key` is valid and the value name is a static wide string.
        let status = unsafe { RegDeleteValueW(key.0, w!("AEC Bridge")) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        status
            .ok()
            .context("failed to disable AEC Bridge at login")?;
        Ok(true)
    }

    pub(super) fn acquire_single_instance() -> Result<SingleInstance> {
        let wide_name: Vec<u16> = SINGLE_INSTANCE_MUTEX_NAME
            .encode_utf16()
            .chain(iter::once(0))
            .collect();
        // CreateMutexW reports an existing named object through last-error even
        // though it successfully returns a handle. Clear stale thread state so
        // a successful new mutex cannot be mistaken for an existing one.
        unsafe { SetLastError(ERROR_SUCCESS) };
        // SAFETY: the name is NUL-terminated and remains alive during the
        // call. The mutex is not initially owned, so no ReleaseMutex is needed.
        let handle_result = unsafe { CreateMutexW(None, false, PCWSTR(wide_name.as_ptr())) };
        // This must be the first operation after CreateMutexW: formatting an
        // error or adding anyhow context could itself invoke Windows APIs.
        let create_status = unsafe { GetLastError() };
        let handle =
            handle_result.context("failed to create the AEC Bridge single-instance mutex")?;
        let already_running = create_status == ERROR_ALREADY_EXISTS;
        if already_running {
            // SAFETY: this process owns the returned handle even when the mutex
            // object already existed.
            unsafe { CloseHandle(handle) }
                .context("failed to close the existing AEC Bridge mutex handle")?;
            Ok(SingleInstance::AlreadyRunning)
        } else {
            Ok(SingleInstance::Acquired(SingleInstanceGuard { handle }))
        }
    }

    pub(super) fn show_already_running_message() -> Result<()> {
        // SAFETY: both strings are static and NUL-terminated; a parent window
        // is intentionally omitted because this runs before the GUI is built.
        let result = unsafe {
            MessageBoxW(
                None,
                w!(
                    "AEC Bridge is already running. Restore it from the Windows taskbar or its notification-area icon near the clock."
                ),
                w!("AEC Bridge"),
                MB_OK | MB_ICONINFORMATION,
            )
        };
        if result.0 == 0 {
            bail!("failed to show the AEC Bridge already-running dialog");
        }
        Ok(())
    }

    pub(super) fn show_startup_error_message(message: &str) -> Result<()> {
        let sanitized = message.replace('\0', "�");
        let wide_message: Vec<u16> = sanitized.encode_utf16().chain(iter::once(0)).collect();
        // SAFETY: the message buffer is NUL-terminated and remains alive for
        // the call; the title is static and NUL-terminated.
        let result = unsafe {
            MessageBoxW(
                None,
                PCWSTR(wide_message.as_ptr()),
                w!("AEC Bridge could not start"),
                MB_OK | MB_ICONERROR,
            )
        };
        if result.0 == 0 {
            bail!("failed to show the AEC Bridge startup-error dialog");
        }
        Ok(())
    }

    fn open_run_key(
        access: windows::Win32::System::Registry::REG_SAM_FLAGS,
    ) -> Result<Option<RegistryKey>> {
        let mut key = HKEY::default();
        // SAFETY: `key` is a valid output pointer and the subkey is a static
        // NUL-terminated wide string.
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
                None,
                access,
                &mut key,
            )
        };
        if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
            return Ok(None);
        }
        status
            .ok()
            .context("failed to open the current user's Run registry key")?;
        Ok(Some(RegistryKey(key)))
    }

    fn create_run_key() -> Result<RegistryKey> {
        let mut key = HKEY::default();
        // SAFETY: `key` is a valid output pointer, the subkey is static, and
        // no optional class or security descriptor is supplied.
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
        }
        .ok()
        .context("failed to create or open the current user's Run registry key")?;
        Ok(RegistryKey(key))
    }

    fn encode_registry_string(value: &str) -> Vec<u8> {
        value
            .encode_utf16()
            .chain(iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn decode_registry_string(bytes: &[u8]) -> Result<String> {
        if !bytes.len().is_multiple_of(2) {
            bail!("AEC Bridge login-startup REG_SZ has an odd byte length");
        }
        let mut wide: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        while wide.last() == Some(&0) {
            wide.pop();
        }
        if wide.contains(&0) {
            bail!("AEC Bridge login-startup REG_SZ contains an embedded NUL");
        }
        String::from_utf16(&wide).context("AEC Bridge login-startup REG_SZ is not valid UTF-16")
    }
}

#[cfg(not(windows))]
mod platform {
    use anyhow::{Result, bail};

    use super::{SingleInstance, SingleInstanceGuard};

    pub(super) fn read_login_startup_value() -> Result<Option<String>> {
        bail!("login-startup registration is only available on Windows")
    }

    pub(super) fn write_login_startup_value(_command: &str) -> Result<()> {
        bail!("login-startup registration is only available on Windows")
    }

    pub(super) fn delete_login_startup_value() -> Result<bool> {
        bail!("login-startup registration is only available on Windows")
    }

    pub(super) fn acquire_single_instance() -> Result<SingleInstance> {
        let _ = SingleInstanceGuard { _private: () };
        bail!("single-instance coordination is only available on Windows")
    }

    pub(super) fn show_already_running_message() -> Result<()> {
        Ok(())
    }

    pub(super) fn show_startup_error_message(message: &str) -> Result<()> {
        eprintln!("{message}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        LOGIN_STARTUP_ARGUMENT, LoginStartupStatus, classify_login_startup, command_for_executable,
    };

    #[test]
    fn login_command_always_quotes_the_full_path() {
        let command =
            command_for_executable(Path::new(r"C:\Program Files\AEC Bridge\aec-bridge.exe"))
                .unwrap();
        assert_eq!(
            command,
            format!(r#""C:\Program Files\AEC Bridge\aec-bridge.exe" {LOGIN_STARTUP_ARGUMENT}"#)
        );
    }

    #[test]
    fn login_command_preserves_unicode() {
        let command = command_for_executable(Path::new(r"C:\Users\Zoë\AEC 桥.exe")).unwrap();
        assert_eq!(command, r#""C:\Users\Zoë\AEC 桥.exe" --login-startup"#);
    }

    #[test]
    fn login_command_rejects_unsupported_characters() {
        let quote_error = command_for_executable(Path::new("C:\\bad\"name.exe")).unwrap_err();
        assert!(quote_error.to_string().contains("double quote"));

        let nul_error = command_for_executable(Path::new("C:\\bad\0name.exe")).unwrap_err();
        assert!(nul_error.to_string().contains("NUL"));
    }

    #[test]
    fn login_command_rejects_a_path_beyond_the_windows_run_limit() {
        let long_name = format!(r"C:\{}\aec-bridge.exe", "a".repeat(260));
        let error = command_for_executable(Path::new(&long_name)).unwrap_err();
        assert!(error.to_string().contains("260-character limit"));
    }

    #[test]
    fn classifies_disabled_current_and_stale_values() {
        let expected = r#""C:\AEC Bridge\aec-bridge.exe" --login-startup"#;
        assert_eq!(
            classify_login_startup(None, expected),
            LoginStartupStatus::Disabled
        );
        assert_eq!(
            classify_login_startup(Some(expected), expected),
            LoginStartupStatus::Current
        );
        assert_eq!(
            classify_login_startup(Some(r#""D:\Old\aec-bridge.exe" --login-startup"#), expected,),
            LoginStartupStatus::Stale {
                registered_command: r#""D:\Old\aec-bridge.exe" --login-startup"#.to_owned(),
            }
        );
    }

    #[test]
    fn extra_or_missing_arguments_are_stale() {
        let expected = r#""C:\AEC Bridge\aec-bridge.exe" --login-startup"#;
        for registered in [
            r#""C:\AEC Bridge\aec-bridge.exe""#,
            r#""C:\AEC Bridge\aec-bridge.exe" --login-startup --extra"#,
        ] {
            assert!(matches!(
                classify_login_startup(Some(registered), expected),
                LoginStartupStatus::Stale { .. }
            ));
        }
    }
}
