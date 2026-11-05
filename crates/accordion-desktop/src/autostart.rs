//! Starting the app when you sign in to the computer, so files keep
//! syncing. It's turned on when the first account is added.

/// The argument the app starts with at sign-in, so it stays in the tray
/// instead of opening its window.
pub const BACKGROUND: &str = "--background";

#[cfg(windows)]
pub fn turn_on() {
    // A build being tried keeps its settings apart, and shouldn't start at
    // sign-in.
    if std::env::var_os("ACCORDION_DESKTOP_HOME").is_some() {
        return;
    }
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
    use windows::core::HSTRING;

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let command = format!("\"{}\" {BACKGROUND}", exe.display());
    let value: Vec<u16> = command.encode_utf16().chain([0]).collect();
    unsafe {
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            &HSTRING::from("Accordion"),
            REG_SZ.0,
            Some(value.as_ptr().cast()),
            (value.len() * 2) as u32,
        );
    }
}

#[cfg(not(windows))]
pub fn turn_on() {}
