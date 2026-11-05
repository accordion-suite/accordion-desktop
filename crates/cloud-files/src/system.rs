//! The calls into Windows.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use windows::Storage::Provider::{
    StorageProviderHardlinkPolicy, StorageProviderHydrationPolicy,
    StorageProviderHydrationPolicyModifier, StorageProviderInSyncPolicy,
    StorageProviderPopulationPolicy, StorageProviderProtectionMode, StorageProviderSyncRootInfo,
    StorageProviderSyncRootManager,
};
use windows::Storage::StorageFolder;
use windows::Win32::Foundation::{
    HANDLE, HLOCAL, LocalFree, NTSTATUS, STATUS_CLOUD_FILE_UNSUCCESSFUL, STATUS_SUCCESS,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::Storage::CloudFilters::*;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_PINNED,
    FILE_ATTRIBUTE_TAG_INFO, FILE_ATTRIBUTE_UNPINNED, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileAttributeTagInfo, GetFileInformationByHandleEx,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW,
    RegOpenKeyExW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{HRESULT, HSTRING, PCWSTR, PWSTR};

use crate::{BLOCK, Fetch, FetchRequest, Placeholder, RootInfo, State, Transfer, file_time};

/// `WRITE_DAC`, which the calls that change a placeholder accept in place
/// of write access, so they work while another app has the file open.
const WRITE_DAC: u32 = 0x0004_0000;

fn error(err: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(err.code().0)
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

// ---- Sync roots -------------------------------------------------------------

/// The ID for an account's sync root. Windows wants the provider, the
/// signed-in Windows user, and the account, separated by `!`.
pub fn root_id(account: &str) -> io::Result<String> {
    Ok(format!("Accordion!{}!{account}", user_sid()?))
}

fn user_sid() -> io::Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(error)?;
        let token = Owned(token);
        let mut needed = 0;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut needed);
        let mut buffer = vec![0u8; needed as usize];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
        .map_err(error)?;
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text).map_err(error)?;
        let sid = text
            .to_string()
            .map_err(|_| io::Error::other("the user's SID isn't text"));
        LocalFree(Some(HLOCAL(text.0.cast())));
        sid
    }
}

struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Registers a sync root, which puts it in File Explorer's navigation
/// pane. Registering one that's there already updates it.
pub fn register(info: &RootInfo) -> io::Result<()> {
    std::fs::create_dir_all(&info.path)?;
    let folder = StorageFolder::GetFolderFromPathAsync(&HSTRING::from(info.path.as_os_str()))
        .and_then(|operation| operation.join())
        .map_err(error)?;
    let root = StorageProviderSyncRootInfo::new().map_err(error)?;
    (|| -> windows::core::Result<()> {
        root.SetId(&HSTRING::from(&info.id))?;
        root.SetPath(&folder)?;
        root.SetDisplayNameResource(&HSTRING::from(&info.display_name))?;
        root.SetIconResource(&HSTRING::from(&info.icon))?;
        root.SetVersion(&HSTRING::from(&info.version))?;
        // A file opened downloads in full, and Storage Sense may free up
        // space by making files placeholders again, as in Google Drive.
        root.SetHydrationPolicy(StorageProviderHydrationPolicy::Full)?;
        root.SetHydrationPolicyModifier(
            StorageProviderHydrationPolicyModifier::AutoDehydrationAllowed,
        )?;
        // The app makes every placeholder itself, from the list of changes,
        // so folders never wait for the server when they're opened.
        root.SetPopulationPolicy(StorageProviderPopulationPolicy::AlwaysFull)?;
        // Only a change to a file's contents or date makes it need syncing.
        // The app's own changes, such as making a file read-only, don't.
        root.SetInSyncPolicy(
            StorageProviderInSyncPolicy::FileLastWriteTime
                | StorageProviderInSyncPolicy::PreserveInsyncForSyncEngine,
        )?;
        root.SetHardlinkPolicy(StorageProviderHardlinkPolicy::None)?;
        root.SetShowSiblingsAsGroup(false)?;
        root.SetAllowPinning(true)?;
        root.SetProtectionMode(StorageProviderProtectionMode::Personal)?;
        StorageProviderSyncRootManager::Register(&root)
    })()
    .map_err(error)
}

/// Removes a sync root from File Explorer. Its files stay, as ordinary
/// files where they were downloaded, and placeholders where they weren't.
pub fn unregister(id: &str) -> io::Result<()> {
    StorageProviderSyncRootManager::Unregister(&HSTRING::from(id)).map_err(error)
}

/// The sync roots Accordion has registered for this Windows user, as IDs
/// and folders, such as to remove ones whose account is gone. Windows only
/// lists an unpackaged app's roots in the registry.
pub fn registered() -> io::Result<Vec<(String, PathBuf)>> {
    const ROOTS: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager";
    let sid = user_sid()?;
    let prefix = format!("Accordion!{sid}!");
    let roots = HSTRING::from(ROOTS);
    let mut key = HKEY::default();
    let status = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, &roots, None, KEY_READ, &mut key) };
    if status.is_err() {
        // No provider has registered a root yet.
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for index in 0.. {
        let mut name = [0u16; 512];
        let mut length = name.len() as u32;
        let status = unsafe {
            RegEnumKeyExW(
                key,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut length,
                None,
                None,
                None,
                None,
            )
        };
        if status.is_err() {
            break;
        }
        let id = String::from_utf16_lossy(&name[..length as usize]);
        if !id.starts_with(&prefix) {
            continue;
        }
        let subkey = HSTRING::from(format!("{ROOTS}\\{id}\\UserSyncRoots"));
        let mut path = [0u16; 1024];
        let mut bytes = (path.len() * 2) as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                &subkey,
                &HSTRING::from(&sid),
                RRF_RT_REG_SZ,
                None,
                Some(path.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if status.is_ok() {
            let length = (bytes as usize / 2).saturating_sub(1);
            found.push((id, PathBuf::from(String::from_utf16_lossy(&path[..length]))));
        }
    }
    unsafe {
        let _ = RegCloseKey(key);
    }
    Ok(found)
}

// ---- Connecting -------------------------------------------------------------

/// The app's connection to a sync root. While it's open, Windows asks the
/// app for placeholders' contents. Dropping it disconnects.
pub struct Connection {
    key: CF_CONNECTION_KEY,
    // Windows hands this back to each callback, so it must stay put until
    // the connection closes.
    _fetch: Box<Arc<dyn Fetch>>,
}

// SAFETY: the key is a number, and the callbacks only read the context.
unsafe impl Send for Connection {}
unsafe impl Sync for Connection {}

impl Drop for Connection {
    fn drop(&mut self) {
        unsafe {
            let _ = CfDisconnectSyncRoot(self.key);
        }
    }
}

static CALLBACKS: [CF_CALLBACK_REGISTRATION; 3] = [
    CF_CALLBACK_REGISTRATION {
        Type: CF_CALLBACK_TYPE_FETCH_DATA,
        Callback: Some(on_fetch),
    },
    CF_CALLBACK_REGISTRATION {
        Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
        Callback: Some(on_cancel),
    },
    CF_CALLBACK_REGISTRATION {
        Type: CF_CALLBACK_TYPE_NONE,
        Callback: None,
    },
];

/// Connects to the sync root at `path`, so `fetch` sends placeholders'
/// contents when something opens them.
pub fn connect(path: &Path, fetch: Arc<dyn Fetch>) -> io::Result<Connection> {
    let fetch = Box::new(fetch);
    let context: *const Arc<dyn Fetch> = &*fetch;
    let key = unsafe {
        CfConnectSyncRoot(
            &HSTRING::from(path.as_os_str()),
            CALLBACKS.as_ptr(),
            Some(context.cast()),
            // The app reading a placeholder of its own, such as to see
            // whether it changed, fails instead of downloading it.
            CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH | CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION,
        )
    }
    .map_err(error)?;
    Ok(Connection { key, _fetch: fetch })
}

unsafe extern "system" fn on_fetch(
    info: *const CF_CALLBACK_INFO,
    parameters: *const CF_CALLBACK_PARAMETERS,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        let info = &*info;
        let data = &(*parameters).Anonymous.FetchData;
        let fetch = &*info.CallbackContext.cast::<Arc<dyn Fetch>>();
        let identity = if info.FileIdentity.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(
                info.FileIdentity.cast::<u8>(),
                info.FileIdentityLength as usize,
            )
            .to_vec()
        };
        let mut path = info.VolumeDosName.to_string().unwrap_or_default();
        path.push_str(&info.NormalizedPath.to_string().unwrap_or_default());
        let offset = u64::try_from(data.RequiredFileOffset).unwrap_or(0);
        let length = u64::try_from(data.RequiredLength).unwrap_or(0);
        fetch.fetch(FetchRequest {
            identity,
            path: PathBuf::from(path),
            offset,
            length,
            file_size: u64::try_from(info.FileSize).unwrap_or(0),
            transfer: Transfer {
                connection: info.ConnectionKey.0,
                key: info.TransferKey,
                request: info.RequestKey,
                offset,
                length,
            },
        });
    }));
}

unsafe extern "system" fn on_cancel(
    info: *const CF_CALLBACK_INFO,
    _parameters: *const CF_CALLBACK_PARAMETERS,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        let info = &*info;
        let fetch = &*info.CallbackContext.cast::<Arc<dyn Fetch>>();
        fetch.cancel(info.TransferKey);
    }));
}

impl Transfer {
    /// Sends part of the file's contents, starting at `offset`, which is
    /// on a [`BLOCK`] boundary. `data` is a whole number of blocks, unless
    /// it ends the file.
    pub fn send(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        debug_assert_eq!(offset % BLOCK, 0);
        self.execute(
            STATUS_SUCCESS,
            data.as_ptr().cast(),
            offset,
            data.len() as u64,
        )
    }

    /// Tells Windows the contents can't come, so the app that opened the
    /// file gets an error instead of waiting.
    pub fn fail(&self) {
        let _ = self.execute(
            STATUS_CLOUD_FILE_UNSUCCESSFUL,
            std::ptr::null(),
            self.offset,
            self.length,
        );
    }

    fn execute(
        &self,
        status: NTSTATUS,
        buffer: *const c_void,
        offset: u64,
        length: u64,
    ) -> io::Result<()> {
        let operation = CF_OPERATION_INFO {
            StructSize: size_of::<CF_OPERATION_INFO>() as u32,
            Type: CF_OPERATION_TYPE_TRANSFER_DATA,
            ConnectionKey: CF_CONNECTION_KEY(self.connection),
            TransferKey: self.key,
            CorrelationVector: std::ptr::null(),
            SyncStatus: std::ptr::null(),
            RequestKey: self.request,
        };
        let mut parameters = CF_OPERATION_PARAMETERS {
            ParamSize: (offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
                + size_of::<CF_OPERATION_PARAMETERS_0_0>()) as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 {
                TransferData: CF_OPERATION_PARAMETERS_0_0 {
                    Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
                    CompletionStatus: status,
                    Buffer: buffer,
                    Offset: offset as i64,
                    Length: length as i64,
                },
            },
        };
        unsafe { CfExecute(&operation, &mut parameters) }.map_err(error)
    }
}

// ---- Placeholders -----------------------------------------------------------

fn metadata(placeholder: &Placeholder) -> CF_FS_METADATA {
    let time = file_time(placeholder.modified);
    CF_FS_METADATA {
        BasicInfo: FILE_BASIC_INFO {
            CreationTime: time,
            LastAccessTime: time,
            LastWriteTime: time,
            ChangeTime: time,
            FileAttributes: if placeholder.folder {
                FILE_ATTRIBUTE_DIRECTORY.0
            } else {
                FILE_ATTRIBUTE_NORMAL.0
            },
        },
        FileSize: if placeholder.folder {
            0
        } else {
            placeholder.size as i64
        },
    }
}

/// Makes placeholders in `dir`, a folder in a sync root, marked as in sync.
/// Folders are made with everything in them known, since the app makes
/// their placeholders too.
pub fn create(dir: &Path, placeholders: &[Placeholder]) -> io::Result<()> {
    let names: Vec<Vec<u16>> = placeholders
        .iter()
        .map(|placeholder| placeholder.name.encode_utf16().chain([0]).collect())
        .collect();
    let mut infos: Vec<CF_PLACEHOLDER_CREATE_INFO> = placeholders
        .iter()
        .zip(&names)
        .map(|(placeholder, name)| CF_PLACEHOLDER_CREATE_INFO {
            RelativeFileName: PCWSTR(name.as_ptr()),
            FsMetadata: metadata(placeholder),
            FileIdentity: placeholder.identity.as_ptr().cast(),
            FileIdentityLength: placeholder.identity.len() as u32,
            Flags: if placeholder.folder {
                CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC
                    | CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION
            } else {
                CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC
            },
            Result: HRESULT(0),
            CreateUsn: 0,
        })
        .collect();
    let dir = wide(dir);
    unsafe { CfCreatePlaceholders(PCWSTR(dir.as_ptr()), &mut infos, CF_CREATE_FLAG_NONE, None) }
        .map_err(error)?;
    match infos.iter().find(|info| info.Result.is_err()) {
        Some(failed) => Err(io::Error::from_raw_os_error(failed.Result.0)),
        None => Ok(()),
    }
}

fn open(path: &Path, change: bool) -> io::Result<File> {
    let access = if change {
        FILE_READ_ATTRIBUTES.0 | WRITE_DAC
    } else {
        FILE_READ_ATTRIBUTES.0
    };
    OpenOptions::new()
        .access_mode(access)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        // Needed to open a folder.
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
}

fn handle(file: &File) -> HANDLE {
    HANDLE(file.as_raw_handle())
}

/// What Windows knows about the file or folder at `path`.
pub fn state(path: &Path) -> io::Result<State> {
    let file = open(path, false)?;
    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            handle(&file),
            FileAttributeTagInfo,
            (&raw mut tag).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    }
    .map_err(error)?;
    let flags =
        unsafe { CfGetPlaceholderStateFromAttributeTag(tag.FileAttributes, tag.ReparseTag) };
    let placeholder = flags.0 & CF_PLACEHOLDER_STATE_PLACEHOLDER.0 != 0;
    let mut in_sync = false;
    if placeholder {
        // The basic information, with room for an identity of up to 4 KiB.
        let mut buffer = vec![0u64; (size_of::<CF_PLACEHOLDER_BASIC_INFO>() + 4096) / 8];
        unsafe {
            CfGetPlaceholderInfo(
                handle(&file),
                CF_PLACEHOLDER_INFO_BASIC,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                None,
            )
        }
        .map_err(error)?;
        let basic = unsafe { &*buffer.as_ptr().cast::<CF_PLACEHOLDER_BASIC_INFO>() };
        in_sync = basic.InSyncState == CF_IN_SYNC_STATE_IN_SYNC;
    }
    Ok(State {
        placeholder,
        in_sync,
        partial: flags.0 & CF_PLACEHOLDER_STATE_PARTIAL.0 != 0,
        pinned: tag.FileAttributes & FILE_ATTRIBUTE_PINNED.0 != 0,
        unpinned: tag.FileAttributes & FILE_ATTRIBUTE_UNPINNED.0 != 0,
    })
}

/// Turns an ordinary file or folder in a sync root into a placeholder that
/// matches the server, such as after uploading it. Its contents stay.
pub fn convert(path: &Path, identity: &[u8]) -> io::Result<()> {
    let file = open(path, true)?;
    unsafe {
        CfConvertToPlaceholder(
            handle(&file),
            Some(identity.as_ptr().cast()),
            identity.len() as u32,
            CF_CONVERT_FLAG_MARK_IN_SYNC,
            None,
            None,
        )
    }
    .map_err(error)
}

/// Brings a placeholder up to date with the server's version: its size and
/// date, and, when `dehydrate` is set, it lets go of the old contents, so
/// opening it fetches the new ones.
pub fn update(path: &Path, placeholder: &Placeholder, dehydrate: bool) -> io::Result<()> {
    let file = open(path, true)?;
    let metadata = metadata(placeholder);
    let flags = if dehydrate {
        CF_UPDATE_FLAG_MARK_IN_SYNC | CF_UPDATE_FLAG_DEHYDRATE
    } else {
        CF_UPDATE_FLAG_MARK_IN_SYNC
    };
    unsafe {
        CfUpdatePlaceholder(
            handle(&file),
            Some(&metadata),
            Some(placeholder.identity.as_ptr().cast()),
            placeholder.identity.len() as u32,
            None,
            flags,
            None,
            None,
        )
    }
    .map_err(error)
}

/// Marks a placeholder as matching the server's version with `identity`,
/// such as after uploading it. Its contents and date stay.
pub fn mark_synced(path: &Path, identity: &[u8]) -> io::Result<()> {
    let file = open(path, true)?;
    unsafe {
        CfUpdatePlaceholder(
            handle(&file),
            None,
            Some(identity.as_ptr().cast()),
            identity.len() as u32,
            None,
            CF_UPDATE_FLAG_MARK_IN_SYNC,
            None,
            None,
        )
    }
    .map_err(error)
}

/// Marks a placeholder as matching the server, or not.
pub fn set_in_sync(path: &Path, in_sync: bool) -> io::Result<()> {
    let file = open(path, true)?;
    let state = if in_sync {
        CF_IN_SYNC_STATE_IN_SYNC
    } else {
        CF_IN_SYNC_STATE_NOT_IN_SYNC
    };
    unsafe { CfSetInSyncState(handle(&file), state, CF_SET_IN_SYNC_FLAG_NONE, None) }.map_err(error)
}

/// Sets whether a file or folder is kept on this computer: pinned for
/// **Always keep on this device**, unpinned for **Free up space**, or
/// neither.
pub fn set_pinned(path: &Path, pinned: Option<bool>) -> io::Result<()> {
    let file = open(path, true)?;
    let state = match pinned {
        Some(true) => CF_PIN_STATE_PINNED,
        Some(false) => CF_PIN_STATE_UNPINNED,
        None => CF_PIN_STATE_UNSPECIFIED,
    };
    unsafe { CfSetPinState(handle(&file), state, CF_SET_PIN_FLAG_NONE, None) }.map_err(error)
}

/// Downloads a placeholder's contents, and waits until they're here.
pub fn hydrate(path: &Path) -> io::Result<()> {
    let file = open(path, true)?;
    unsafe { CfHydratePlaceholder(handle(&file), 0, -1, CF_HYDRATE_FLAG_NONE, None) }.map_err(error)
}

/// Lets go of a placeholder's contents, which stay on the server.
pub fn dehydrate(path: &Path) -> io::Result<()> {
    let file = open(path, true)?;
    unsafe { CfDehydratePlaceholder(handle(&file), 0, -1, CF_DEHYDRATE_FLAG_NONE, None) }
        .map_err(error)
}
