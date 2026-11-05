//! The accounts this computer syncs, and where the app keeps what it knows.
//!
//! Each account's address, name, and folder are in `accounts.json`, in the
//! app's settings folder. Its session token and tunnel key are secrets, so
//! they're in the system's own store: Windows' Credential Manager, or a
//! file only you can read elsewhere.

use std::path::PathBuf;

use accordion_sync::tunnel::TunnelSetup;
use serde::{Deserialize, Serialize};

/// An account, without its secrets.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    /// A short ID the app picks, for its files and its sync root.
    pub id: String,
    /// What the person calls the server, such as "Home".
    pub name: String,
    /// The server's address, such as `https://ourhome.dedyn.io`.
    pub server: String,
    /// The synced folder, such as `C:\Users\Sam\Accordion (Home)`.
    pub folder: PathBuf,
    /// This computer's name, as the server and conflict copies show it.
    pub device: String,
    pub username: String,
    pub display_name: String,
}

/// An account's secrets.
#[derive(Clone, Serialize, Deserialize)]
pub struct Secrets {
    pub token: String,
    /// The account's own tunnel, once the server has remote access on.
    #[serde(default)]
    pub tunnel: Option<TunnelSetup>,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    accounts: Vec<Account>,
}

/// The app's settings folder: `%APPDATA%\Accordion` on Windows.
/// `ACCORDION_DESKTOP_HOME` puts everything in one folder instead, such as
/// for trying a build.
pub fn settings_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("ACCORDION_DESKTOP_HOME") {
        return PathBuf::from(home);
    }
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    base.unwrap_or_else(|| PathBuf::from(".")).join("Accordion")
}

/// Where each account's sync state goes: `%LOCALAPPDATA%\Accordion` on
/// Windows, since it belongs to this computer and shouldn't roam.
pub fn state_path(account: &Account) -> PathBuf {
    let dir = if std::env::var_os("ACCORDION_DESKTOP_HOME").is_some() {
        settings_dir()
    } else {
        #[cfg(windows)]
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        #[cfg(not(windows))]
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            });
        base.unwrap_or_else(|| PathBuf::from(".")).join("Accordion")
    };
    dir.join("state").join(format!("{}.db", account.id))
}

/// The folder for a new account named `name`, in the person's own folder.
pub fn folder_for(name: &str) -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(format!("Accordion ({name})"))
}

fn accounts_path() -> PathBuf {
    settings_dir().join("accounts.json")
}

pub fn load() -> Result<Vec<Account>, String> {
    match std::fs::read(accounts_path()) {
        Ok(bytes) => serde_json::from_slice::<File>(&bytes)
            .map(|file| file.accounts)
            .map_err(|err| format!("Accordion couldn't read its list of accounts ({err}).")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(format!(
            "Accordion couldn't read its list of accounts ({err})."
        )),
    }
}

pub fn save(accounts: &[Account]) -> Result<(), String> {
    let path = accounts_path();
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(settings_dir())?;
        let json = serde_json::to_vec_pretty(&File {
            accounts: accounts.to_vec(),
        })?;
        let temp = path.with_extension("json.new");
        std::fs::write(&temp, json)?;
        std::fs::rename(&temp, &path)
    };
    write().map_err(|err| format!("Accordion couldn't save its list of accounts ({err})."))
}

/// A new account ID: 12 letters and digits.
pub fn new_id() -> String {
    let (key, _) = accordion_sync::tunnel::new_key();
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .take(12)
        .collect()
}

/// The name of a server's host, such as `ourhome.dedyn.io`.
pub fn host_of(origin: &str) -> String {
    origin
        .split("://")
        .nth(1)
        .unwrap_or(origin)
        .split(['/', ':'])
        .next()
        .unwrap_or("Accordion")
        .to_owned()
}

/// This computer's name, as Windows has it.
pub fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Computer".to_owned())
}

// ---- Secrets -----------------------------------------------------------------

pub fn load_secrets(account: &Account) -> Result<Secrets, String> {
    let bytes = secrets::read(&account.id)
        .map_err(|err| {
            format!(
                "Accordion couldn't read the sign-in for {} ({err}).",
                account.name
            )
        })?
        .ok_or_else(|| {
            format!(
                "Accordion's sign-in for {} is missing. Remove the account, then add it again.",
                account.name
            )
        })?;
    serde_json::from_slice(&bytes).map_err(|err| {
        format!(
            "Accordion couldn't read the sign-in for {} ({err}).",
            account.name
        )
    })
}

pub fn save_secrets(account: &Account, secrets: &Secrets) -> Result<(), String> {
    let json = serde_json::to_vec(secrets).expect("secrets are plain data");
    secrets::write(&account.id, &json).map_err(|err| {
        format!(
            "Accordion couldn't save the sign-in for {} ({err}).",
            account.name
        )
    })
}

pub fn forget_secrets(account: &Account) {
    let _ = secrets::delete(&account.id);
}

#[cfg(windows)]
mod secrets {
    //! Windows' Credential Manager, where each account's secrets are a
    //! generic credential named `Accordion/<id>`, readable only by this
    //! Windows user.

    use std::io;

    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
        CredReadW, CredWriteW,
    };
    use windows::core::{HSTRING, PWSTR};

    fn target(id: &str) -> HSTRING {
        HSTRING::from(format!("Accordion/{id}"))
    }

    pub fn write(id: &str, secret: &[u8]) -> io::Result<()> {
        let target = target(id);
        let mut user: Vec<u16> = "Accordion".encode_utf16().chain([0]).collect();
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_ptr().cast_mut()),
            CredentialBlobSize: secret.len() as u32,
            CredentialBlob: secret.as_ptr().cast_mut(),
            // Kept on this computer only, never roamed with the profile.
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: PWSTR(user.as_mut_ptr()),
            ..Default::default()
        };
        unsafe { CredWriteW(&credential, 0) }
            .map_err(|err| io::Error::from_raw_os_error(err.code().0))
    }

    pub fn read(id: &str) -> io::Result<Option<Vec<u8>>> {
        let mut found: *mut CREDENTIALW = std::ptr::null_mut();
        match unsafe { CredReadW(&target(id), CRED_TYPE_GENERIC, None, &mut found) } {
            Ok(()) => unsafe {
                let credential = &*found;
                let secret = std::slice::from_raw_parts(
                    credential.CredentialBlob,
                    credential.CredentialBlobSize as usize,
                )
                .to_vec();
                CredFree(found.cast());
                Ok(Some(secret))
            },
            Err(err) if err.code() == ERROR_NOT_FOUND.to_hresult() => Ok(None),
            Err(err) => Err(io::Error::from_raw_os_error(err.code().0)),
        }
    }

    pub fn delete(id: &str) -> io::Result<()> {
        unsafe { CredDeleteW(&target(id), CRED_TYPE_GENERIC, None) }
            .map_err(|err| io::Error::from_raw_os_error(err.code().0))
    }
}

#[cfg(not(windows))]
mod secrets {
    //! Until the app uses macOS's Keychain and Linux's Secret Service,
    //! secrets are in a file only you can read.

    use std::io;
    use std::path::PathBuf;

    fn path(id: &str) -> PathBuf {
        super::settings_dir().join("secrets").join(id)
    }

    pub fn write(id: &str, secret: &[u8]) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = path(id);
        std::fs::create_dir_all(path.parent().expect("it's in a folder"))?;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?
            .write_all(secret)
    }

    pub fn read(id: &str) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(path(id)) {
            Ok(secret) => Ok(Some(secret)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub fn delete(id: &str) -> io::Result<()> {
        std::fs::remove_file(path(id))
    }
}
