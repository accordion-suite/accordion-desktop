//! Files on demand on Windows (decision 23): each account is a sync root in
//! File Explorer, files arrive as placeholders, and their contents download
//! when something opens them.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use accordion_sync::{Client, Error, OnDemand, RemoteItem, Status};
use cloud_files::{BLOCK, Fetch, FetchRequest, Placeholder, RootInfo};

use crate::accounts::Account;

/// Contents go to Windows in pieces of this size, a whole number of
/// blocks.
const PIECE: usize = 1024 * 1024;

/// What a placeholder knows its item by: its ID and its version's SHA-256,
/// so opening it fetches that version.
fn identity(item: &RemoteItem) -> Vec<u8> {
    format!(
        "{}\n{}",
        item.id,
        item.sha256.as_deref().unwrap_or_default()
    )
    .into_bytes()
}

fn modified(item: &RemoteItem) -> SystemTime {
    item.modified_at
        .parse::<jiff::Timestamp>()
        .map_or_else(|_| SystemTime::now(), SystemTime::from)
}

/// Puts an account in File Explorer: registers its sync root, and connects
/// to it, so opening a placeholder fetches its contents from `client`. The
/// connection lasts as long as what's returned.
pub fn start(account: &Account, client: Client) -> io::Result<cloud_files::Connection> {
    let exe = std::env::current_exe()?;
    cloud_files::register(&RootInfo {
        id: cloud_files::root_id(&account.id)?,
        path: account.folder.clone(),
        display_name: format!("Accordion ({})", account.name),
        icon: format!("{},0", exe.display()),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })?;
    cloud_files::connect(
        &account.folder,
        Arc::new(Fetcher {
            client,
            runtime: tokio::runtime::Handle::current(),
            fetching: Mutex::default(),
        }),
    )
}

/// Takes an account out of File Explorer. Files whose contents are on this
/// computer stay, as ordinary files. Placeholders whose contents aren't
/// here would only give errors, so they go.
pub fn stop(account: &Account) -> io::Result<()> {
    remove_placeholders(&account.folder);
    cloud_files::unregister(&cloud_files::root_id(&account.id)?)
}

fn remove_placeholders(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            remove_placeholders(&path);
            let _ = std::fs::remove_dir(&path);
        } else if cloud_files::state(&path).is_ok_and(|state| state.partial) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The engine's side of files on demand.
pub struct Files;

impl Files {
    /// A folder that isn't a placeholder yet, such as `Shared with me`,
    /// which the app makes itself, becomes one, so placeholders can go in
    /// it and File Explorer shows it as synced.
    fn prepare(&self, dir: &Path) -> io::Result<()> {
        if !cloud_files::state(dir)?.placeholder {
            let name = dir.file_name().unwrap_or_default().to_string_lossy();
            cloud_files::convert(dir, format!("folder\n{name}").as_bytes())?;
        }
        Ok(())
    }
}

impl OnDemand for Files {
    fn place(&self, item: &RemoteItem, at: &Path) -> io::Result<()> {
        let name = at
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::other("the name isn't text"))?;
        let identity = identity(item);
        let placeholder = Placeholder {
            name,
            identity: &identity,
            folder: item.is_folder(),
            size: item.size.unwrap_or(0),
            modified: modified(item),
        };
        let dir = at.parent().expect("paths are inside the root");
        if std::fs::symlink_metadata(at).is_err() {
            // The sync root itself can't become a placeholder, and needn't.
            let _ = self.prepare(dir);
            return cloud_files::create(dir, &[placeholder]);
        }
        let state = cloud_files::state(at)?;
        if !state.placeholder {
            cloud_files::convert(at, &identity)?;
        }
        if item.is_folder() {
            return cloud_files::update(at, &placeholder, false);
        }
        // A file kept on this device lets go of the old contents too, then
        // downloads the new ones.
        let keep = state.pinned;
        if keep {
            cloud_files::set_pinned(at, None)?;
        }
        let updated = cloud_files::update(at, &placeholder, true);
        if keep {
            cloud_files::set_pinned(at, Some(true))?;
            hydrate_later(at.to_owned());
        }
        updated
    }

    fn synced(&self, item: &RemoteItem, at: &Path) -> io::Result<()> {
        let identity = identity(item);
        if cloud_files::state(at)?.placeholder {
            cloud_files::mark_synced(at, &identity)
        } else {
            if let Some(dir) = at.parent() {
                let _ = self.prepare(dir);
            }
            cloud_files::convert(at, &identity)
        }
    }

    fn status(&self, at: &Path) -> Status {
        match cloud_files::state(at) {
            Ok(state) if !state.placeholder => Status::Ordinary,
            Ok(state) if state.partial => Status::NotHere,
            Ok(state) if state.in_sync => Status::InSync,
            Ok(_) => Status::Changed,
            Err(_) => Status::Ordinary,
        }
    }
}

fn hydrate_later(at: PathBuf) {
    std::thread::spawn(move || {
        let _ = cloud_files::hydrate(&at);
    });
}

/// Keeps what File Explorer's **Always keep on this device** and **Free up
/// space** ask for: downloads pinned files that aren't here, and lets go of
/// unpinned ones that are. Windows leaves both to the app.
pub fn keep_as_asked(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            keep_as_asked(&path);
            continue;
        }
        let Ok(state) = cloud_files::state(&path) else {
            continue;
        };
        if !state.placeholder {
            continue;
        }
        if state.pinned && state.partial {
            let _ = cloud_files::hydrate(&path);
        } else if state.unpinned && !state.partial && state.in_sync {
            let _ = cloud_files::dehydrate(&path);
        }
    }
}

/// Sends placeholders' contents when something opens them.
struct Fetcher {
    client: Client,
    runtime: tokio::runtime::Handle,
    /// Fetches under way, by Windows' transfer key, so they can stop.
    fetching: Mutex<HashMap<i64, tokio::task::AbortHandle>>,
}

impl Fetch for Fetcher {
    fn fetch(&self, request: FetchRequest) {
        let client = self.client.clone();
        let key = request.transfer.key();
        let task = self.runtime.spawn(async move {
            if send(&client, &request).await.is_err() {
                request.transfer.fail();
            }
        });
        let mut fetching = self.fetching.lock().unwrap_or_else(|e| e.into_inner());
        fetching.retain(|_, task| !task.is_finished());
        fetching.insert(key, task.abort_handle());
    }

    fn cancel(&self, transfer: i64) {
        let fetching = self.fetching.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(task) = fetching.get(&transfer) {
            task.abort();
        }
    }
}

/// Streams a file's contents to Windows, from the block that holds the
/// first byte it needs to the end of the file.
async fn send(client: &Client, request: &FetchRequest) -> Result<(), Error> {
    let identity = String::from_utf8_lossy(&request.identity);
    let (id, sha256) = identity.split_once('\n').unwrap_or((&identity, ""));
    let start = request.offset - request.offset % BLOCK;
    let mut contents = client.contents(id, start, sha256).await?;
    let mut at = start;
    let mut buffer: Vec<u8> = Vec::with_capacity(PIECE * 2);
    let local = |err: io::Error| Error::Local(err);
    while let Some(data) = contents.next().await? {
        buffer.extend_from_slice(&data);
        if buffer.len() >= PIECE {
            let whole = buffer.len() - buffer.len() % BLOCK as usize;
            request.transfer.send(at, &buffer[..whole]).map_err(local)?;
            at += whole as u64;
            buffer.drain(..whole);
        }
    }
    // The end of the file, which needn't be a whole block.
    if !buffer.is_empty() {
        request.transfer.send(at, &buffer).map_err(local)?;
    }
    Ok(())
}
