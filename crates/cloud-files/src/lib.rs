//! Windows' Cloud Files API, the system feature OneDrive and Google Drive
//! use (decision 23 in the `accordion` repo's `docs/decisions`).
//!
//! Each account is a sync root: a folder that File Explorer shows in its
//! navigation pane. Files in it can be placeholders, which look like the
//! file, with its name, size, and date, but whose contents stay on the
//! server until something opens them. Windows then asks the app for the
//! contents ([`Fetch`]), and keeps them. **Free up space** in File Explorer
//! makes a file a placeholder again, and **Always keep on this device**
//! pins it.
//!
//! The engine in `accordion-sync` decides what goes where. This crate only
//! talks to Windows.

#[cfg(windows)]
mod system;

#[cfg(windows)]
pub use crate::system::*;

use std::time::SystemTime;

/// A sync root to register with Windows.
#[derive(Clone, Debug)]
pub struct RootInfo {
    /// The root's ID, from [`root_id`].
    pub id: String,
    /// The folder, such as `C:\Users\Sam\Accordion (Home)`.
    pub path: std::path::PathBuf,
    /// The name File Explorer shows, such as **Accordion (Home)**.
    pub display_name: String,
    /// The icon, as a resource such as `C:\...\Accordion.exe,0`.
    pub icon: String,
    /// The app's version. Windows asks again when it changes.
    pub version: String,
}

/// A placeholder to make.
#[derive(Clone, Debug)]
pub struct Placeholder<'a> {
    /// Its name, inside the folder it's made in.
    pub name: &'a str,
    /// What the app knows it by, such as the item's ID in Accordion.
    /// Windows hands it back when it asks for the contents.
    pub identity: &'a [u8],
    pub folder: bool,
    /// The file's size, in bytes. Folders have none.
    pub size: u64,
    pub modified: SystemTime,
}

/// What Windows knows about a file or folder in a sync root.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// It's a placeholder, not an ordinary file the app hasn't taken yet.
    pub placeholder: bool,
    /// The app said it matches the server, and nothing changed it since.
    pub in_sync: bool,
    /// Some or all of its contents aren't on this computer.
    pub partial: bool,
    /// **Always keep on this device.**
    pub pinned: bool,
    /// **Free up space.**
    pub unpinned: bool,
}

/// Something that opened a placeholder needs its contents.
pub trait Fetch: Send + Sync + 'static {
    /// Sends the file's contents with [`Transfer::send`], from
    /// `request.offset` for at least `request.length` bytes, or fails it
    /// with [`Transfer::fail`]. It's called on a thread of Windows', so it
    /// should hand the work to another thread and return.
    fn fetch(&self, request: FetchRequest);

    /// Windows no longer needs a fetch, such as when the app that opened
    /// the file closed it.
    fn cancel(&self, _transfer: i64) {}
}

/// What [`Fetch::fetch`] is asked for.
#[derive(Debug)]
pub struct FetchRequest {
    /// The placeholder's identity, as it was made.
    pub identity: Vec<u8>,
    /// The file, for messages.
    pub path: std::path::PathBuf,
    /// Where the contents are needed from, and how much.
    pub offset: u64,
    pub length: u64,
    /// The file's size, as its placeholder says.
    pub file_size: u64,
    pub transfer: Transfer,
}

/// Where a fetch's contents go. It can be sent to another thread.
#[derive(Debug)]
pub struct Transfer {
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) connection: i64,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) key: i64,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) request: i64,
    /// What was asked for, for failing it.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) offset: u64,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) length: u64,
}

impl Transfer {
    /// The key Windows uses for this fetch, as [`Fetch::cancel`] gets it.
    pub fn key(&self) -> i64 {
        self.key
    }
}

/// Windows' file times count 100-nanosecond steps since 1601.
pub fn file_time(time: SystemTime) -> i64 {
    const UNIX_EPOCH_AS_FILE_TIME: i64 = 116_444_736_000_000_000;
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(since) => {
            UNIX_EPOCH_AS_FILE_TIME + i64::try_from(since.as_nanos() / 100).unwrap_or(i64::MAX / 2)
        }
        Err(before) => {
            UNIX_EPOCH_AS_FILE_TIME - i64::try_from(before.duration().as_nanos() / 100).unwrap_or(0)
        }
    }
}

/// Contents go to Windows in pieces that start on a 4 KiB boundary, as it
/// asks.
pub const BLOCK: u64 = 4096;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn converts_times_to_windows_ones() {
        assert_eq!(file_time(SystemTime::UNIX_EPOCH), 116_444_736_000_000_000);
        assert_eq!(
            file_time(SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
            116_444_736_010_000_000
        );
    }
}
