//! Keeps each account in sync, and what the window and tray show about it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use accordion_sync::add::Added;
use accordion_sync::tunnel::{self, TunnelSetup};
use accordion_sync::{Client, Engine, Error, Reach, State, Via};
use http_body_util::BodyExt;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::{Notify, mpsc};

use crate::accounts::{self, Account, Secrets};

/// How often to look for changes on this computer, and on the server if
/// live updates aren't coming through.
const EVERY: Duration = Duration::from_secs(5);
/// How long to wait before trying an unreachable server again.
const RETRY: Duration = Duration::from_secs(30);
/// How many recent changes each account shows.
const RECENT: usize = 20;

/// What the window shows about an account.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    /// "starting", "synced", "offline", "signed_out", or "problem".
    pub state: &'static str,
    /// What's wrong, for "offline", "signed_out", and "problem".
    pub message: Option<String>,
    /// Things that need the person's attention, such as a conflict copy,
    /// newest first. They stay until the person dismisses them.
    pub notices: Vec<String>,
    /// What changed lately, newest first.
    pub recent: Vec<String>,
    /// Deletes waiting for the person to allow them.
    pub paused_deletes: usize,
    /// "tunnel" when it reaches the server away from home.
    pub via: Option<&'static str>,
    /// Why it can't reach the server away from home yet, from adding it.
    pub away: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct AccountView {
    #[serde(flatten)]
    pub account: Account,
    pub status: Status,
}

/// Asks an account's sync to do something between passes.
enum Control {
    SyncNow,
    AllowDeletes,
    BringBack,
}

struct Running {
    control: mpsc::UnboundedSender<Control>,
    task: tauri::async_runtime::JoinHandle<()>,
    // The account's place in File Explorer, while it's syncing.
    #[cfg(windows)]
    _connection: Option<cloud_files::Connection>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Default)]
struct Inner {
    accounts: Vec<Account>,
    status: HashMap<String, Status>,
    running: HashMap<String, Running>,
}

/// Everything the app is syncing.
pub struct App {
    handle: AppHandle,
    inner: Mutex<Inner>,
    /// Stops a sign-in through the browser that's waiting.
    pub cancel_sign_in: Notify,
}

impl App {
    pub fn new(handle: AppHandle) -> Result<Arc<Self>, String> {
        let accounts = accounts::load()?;
        Ok(Arc::new(Self {
            handle,
            inner: Mutex::new(Inner {
                accounts,
                ..Inner::default()
            }),
            cancel_sign_in: Notify::new(),
        }))
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|err| err.into_inner())
    }

    pub fn accounts(&self) -> Vec<Account> {
        self.inner().accounts.clone()
    }

    pub fn views(&self) -> Vec<AccountView> {
        let inner = self.inner();
        inner
            .accounts
            .iter()
            .map(|account| AccountView {
                account: account.clone(),
                status: inner.status.get(&account.id).cloned().unwrap_or_default(),
            })
            .collect()
    }

    pub fn account(&self, id: &str) -> Option<Account> {
        self.inner()
            .accounts
            .iter()
            .find(|account| account.id == id)
            .cloned()
    }

    /// Tells the window and the tray that something changed.
    fn changed(&self) {
        let _ = self.handle.emit("changed", ());
        crate::tray::update(&self.handle, &self.views());
    }

    /// Updates an account's status. The window and tray hear about it
    /// only if something changed, since each pass sets it, and redrawing
    /// would close what the person opened, such as the tray menu.
    fn set_status(&self, id: &str, update: impl FnOnce(&mut Status)) {
        let changed = {
            let mut inner = self.inner();
            let status = inner.status.entry(id.to_owned()).or_default();
            let before = status.clone();
            update(status);
            *status != before
        };
        if changed {
            self.changed();
        }
    }

    /// Starts syncing every account.
    pub fn start_all(self: &Arc<Self>) {
        for account in self.accounts() {
            self.start(account);
        }
    }

    fn start(self: &Arc<Self>, account: Account) {
        self.set_status(&account.id, |status| status.state = "starting");
        let app = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            if let Err(message) = app.run(account.clone()).await {
                app.set_status(&account.id, |status| {
                    status.state = "problem";
                    status.message = Some(message);
                });
            }
        });
    }

    /// Sets up an account's sync, and starts it.
    async fn run(self: &Arc<Self>, account: Account) -> Result<(), String> {
        let mut secrets = accounts::load_secrets(&account)?;
        // The tunnel follows the server's setup, as of now.
        let tunnel = refresh_tunnel(&account, &secrets).await;
        if tunnel != secrets.tunnel {
            secrets.tunnel = tunnel;
            accounts::save_secrets(&account, &secrets)?;
        }
        let client = Client::with_reach(
            &account.server,
            Some(secrets.token.clone()),
            Reach {
                tunnel: secrets.tunnel.clone(),
                ..Reach::default()
            },
        )
        .map_err(|err| err.to_string())?;
        let state_path = accounts::state_path(&account);
        if let Some(dir) = state_path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|err| format!("Accordion couldn't make {} ({err}).", dir.display()))?;
        }
        let state = State::open(&state_path).map_err(|err| {
            format!(
                "Accordion couldn't open what it knows about {} ({err}).",
                account.name
            )
        })?;
        std::fs::create_dir_all(&account.folder).map_err(|err| {
            format!(
                "Accordion couldn't make {} ({err}).",
                account.folder.display()
            )
        })?;
        let engine = Engine::new(
            client.clone(),
            state,
            account.folder.clone(),
            account.device.clone(),
        );
        #[cfg(windows)]
        let (engine, connection) = {
            let connection = crate::files::start(&account, client.clone()).map_err(|err| {
                format!(
                    "Accordion couldn't add {} to File Explorer ({err}).",
                    account.name
                )
            })?;
            (
                engine.with_on_demand(Arc::new(crate::files::Files)),
                connection,
            )
        };
        let (control, controls) = mpsc::unbounded_channel();
        let app = Arc::clone(self);
        let task = tauri::async_runtime::spawn(keep_in_sync(
            app,
            account.clone(),
            engine,
            client,
            controls,
        ));
        self.inner().running.insert(
            account.id.clone(),
            Running {
                control,
                task,
                #[cfg(windows)]
                _connection: Some(connection),
            },
        );
        Ok(())
    }

    fn send(&self, id: &str, control: Control) {
        if let Some(running) = self.inner().running.get(id) {
            let _ = running.control.send(control);
        }
    }

    pub fn dismiss_notices(&self, id: &str) {
        self.set_status(id, |status| status.notices.clear());
    }

    pub fn sync_now(&self, id: &str) {
        self.send(id, Control::SyncNow);
    }

    pub fn allow_deletes(&self, id: &str) {
        self.send(id, Control::AllowDeletes);
    }

    pub fn bring_back(&self, id: &str) {
        self.send(id, Control::BringBack);
    }

    /// Adds an account that just signed in, and starts syncing it.
    pub fn add(
        self: &Arc<Self>,
        name: &str,
        device: String,
        added: Added,
    ) -> Result<AccountView, String> {
        let name = name.trim();
        let name = if name.is_empty() {
            accounts::host_of(&added.server)
        } else {
            name.to_owned()
        };
        let folder = accounts::folder_for(&name);
        let account = Account {
            id: accounts::new_id(),
            name: name.clone(),
            server: added.server,
            folder,
            device,
            username: added.signed_in.username,
            display_name: added.signed_in.display_name,
        };
        {
            let mut inner = self.inner();
            if let Some(other) = inner.accounts.iter().find(|other| {
                other.name.eq_ignore_ascii_case(&account.name) || other.folder == account.folder
            }) {
                return Err(format!(
                    "There's already an account named {}. Choose another name.",
                    other.name
                ));
            }
            accounts::save_secrets(
                &account,
                &Secrets {
                    token: added.signed_in.token,
                    tunnel: added.tunnel,
                },
            )?;
            inner.accounts.push(account.clone());
            accounts::save(&inner.accounts)?;
        }
        crate::autostart::turn_on();
        if let Some(problem) = added.signed_in.tunnel_problem {
            self.set_status(&account.id, |status| {
                status.away = Some(format!(
                    "This computer syncs at home, but not away from home yet: {problem}"
                ));
            });
        }
        self.start(account.clone());
        Ok(AccountView {
            account: account.clone(),
            status: self
                .inner()
                .status
                .get(&account.id)
                .cloned()
                .unwrap_or_default(),
        })
    }

    /// Stops syncing an account, signs this computer out of it, and
    /// forgets it. Its files that are on this computer stay.
    pub async fn remove(&self, id: &str) -> Result<(), String> {
        let Some(account) = self.account(id) else {
            return Ok(());
        };
        let secrets = accounts::load_secrets(&account).ok();
        {
            let mut inner = self.inner();
            inner.accounts.retain(|account| account.id != id);
            accounts::save(&inner.accounts)?;
            inner.running.remove(id);
            inner.status.remove(id);
        }
        #[cfg(windows)]
        if let Err(err) = crate::files::stop(&account) {
            eprintln!("couldn't take {} out of File Explorer: {err}", account.name);
        }
        // Best effort: if the server can't be reached, the session stays
        // listed in Accordion's Settings, where it can be signed out.
        if let Some(secrets) = &secrets
            && let Ok(client) = Client::with_reach(
                &account.server,
                Some(secrets.token.clone()),
                Reach {
                    tunnel: secrets.tunnel.clone(),
                    ..Reach::default()
                },
            )
        {
            let _ = tokio::time::timeout(Duration::from_secs(10), client.sign_out()).await;
        }
        accounts::forget_secrets(&account);
        let _ = std::fs::remove_file(accounts::state_path(&account));
        self.changed();
        Ok(())
    }
}

/// Registers the account's tunnel key again, so its setup follows the
/// server's address and port, or registers a new key if remote access was
/// turned on since. Unchanged if the server can't be reached.
async fn refresh_tunnel(account: &Account, secrets: &Secrets) -> Option<TunnelSetup> {
    let client = Client::with_reach(
        &account.server,
        Some(secrets.token.clone()),
        Reach {
            tunnel: secrets.tunnel.clone(),
            ..Reach::default()
        },
    )
    .ok()?;
    let private_key = secrets
        .tunnel
        .as_ref()
        .map_or_else(|| tunnel::new_key().0, |tunnel| tunnel.private_key.clone());
    let public_key = tunnel::public_key(&private_key)?;
    match client.register_tunnel(&public_key).await {
        Ok(reply) => Some(reply.with_key(private_key)),
        Err(_) => secrets.tunnel.clone(),
    }
}

/// Syncs one account until it's removed or the app quits: as soon as live
/// updates say something changed, and every few seconds for this
/// computer's changes.
async fn keep_in_sync(
    app: Arc<App>,
    account: Account,
    mut engine: Engine,
    client: Client,
    mut controls: mpsc::UnboundedReceiver<Control>,
) {
    let id = account.id.clone();
    let changed = Arc::new(Notify::new());
    let listening = tokio::spawn(listen(client.clone(), Arc::clone(&changed)));
    let _stop_listening = AbortOnDrop(listening.abort_handle());
    loop {
        match engine.sync().await {
            Ok(report) => {
                let via = match client.via() {
                    Some(Via::Tunnel) => Some("tunnel"),
                    _ => None,
                };
                app.set_status(&id, |status| {
                    status.state = "synced";
                    status.message = None;
                    status.via = via;
                    status.paused_deletes = report.paused_deletes;
                    // A problem that stays, such as a file that can't
                    // upload, is shown once.
                    for notice in &report.notices {
                        if !status.notices.contains(notice) {
                            status.notices.insert(0, notice.clone());
                        }
                    }
                    status.notices.truncate(RECENT);
                    for line in report.done.iter().rev() {
                        status.recent.insert(0, line.clone());
                    }
                    status.recent.truncate(RECENT);
                });
                #[cfg(windows)]
                {
                    let folder = account.folder.clone();
                    let _ =
                        tokio::task::spawn_blocking(move || crate::files::keep_as_asked(&folder))
                            .await;
                }
            }
            Err(Error::SignedOut) => {
                app.set_status(&id, |status| {
                    status.state = "signed_out";
                    status.message = Some(format!(
                        "This computer was signed out of {}. Remove the account, then add it \
                         again.",
                        account.name
                    ));
                });
                return;
            }
            Err(err) if err.is_temporary() => {
                let away = if client.has_tunnel() {
                    " If you're away from home, the network you're on may be blocking \
                     Accordion's tunnel."
                } else {
                    ""
                };
                app.set_status(&id, |status| {
                    status.state = "offline";
                    status.message = Some(format!(
                        "{err}{away} Accordion tries again every 30 seconds."
                    ));
                });
                wait(&mut engine, &mut controls, &changed, RETRY).await;
                continue;
            }
            Err(err) => {
                app.set_status(&id, |status| {
                    status.state = "problem";
                    status.message = Some(err.to_string());
                });
            }
        }
        if !wait(&mut engine, &mut controls, &changed, EVERY).await {
            return;
        }
    }
}

/// Waits for the next pass. Returns `false` once the app no longer wants
/// this account synced.
async fn wait(
    engine: &mut Engine,
    controls: &mut mpsc::UnboundedReceiver<Control>,
    changed: &Notify,
    every: Duration,
) -> bool {
    tokio::select! {
        control = controls.recv() => match control {
            None => return false,
            Some(Control::SyncNow) => {}
            Some(Control::AllowDeletes) => engine.allow_deletes = true,
            Some(Control::BringBack) => {
                let _ = engine.bring_back();
            }
        },
        () = changed.notified() => {}
        () = tokio::time::sleep(every) => {}
    }
    true
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Wakes the account's sync when live updates say Drive changed. If the
/// stream drops, it opens it again.
async fn listen(client: Client, changed: Arc<Notify>) {
    loop {
        if let Ok(mut body) = client.live().await {
            while let Some(Ok(frame)) = body.frame().await {
                if let Ok(data) = frame.into_data()
                    && data.windows(6).any(|window| window == b"drive.")
                {
                    changed.notify_one();
                }
            }
        }
        tokio::time::sleep(EVERY).await;
    }
}
