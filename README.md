# Accordion Desktop

The Accordion desktop app for Windows, macOS, and Linux: it keeps your Drive files in sync with your own server, in File Explorer and Finder. It's built with Tauri over the shared sync engine in `accordion-core`'s `accordion-sync` crate ([decision 23](https://github.com/accordion-suite/accordion/blob/main/docs/decisions/0023-sync.md)).

The Windows app is in early testing. The macOS app is planned. On Linux, `accordion-sync` in `accordion-core` is the command-line client that syncs a plain folder.

This repo is part of [Accordion](https://github.com/accordion-suite/accordion), a self-hosted, zero-telemetry replacement for Google Workspace. Please open issues in the [main repo](https://github.com/accordion-suite/accordion/issues).

## What it does on Windows

- **Each account is in File Explorer's navigation pane,** named like **Accordion (Home)**, with **My Drive** and **Shared with me** inside.
- **Files download when you open them.** Until then, each one is a placeholder with its name, size, and date. **Always keep on this device** and **Free up space** in File Explorer's menu work as they do for OneDrive.
- **The app lives in the system tray,** and starts when you sign in to Windows. Closing its window leaves it syncing. **Quit Accordion** in the tray menu stops it.
- **Adding an account** signs in through your browser, or with a code from Accordion's **Settings** > **Apps and devices**. Away from home, the whole setup under the QR code works too. Each account reaches its server through its own tunnel when you're away.
- **Session tokens and tunnel keys are in Windows' Credential Manager.** The list of accounts is in `%APPDATA%\Accordion`, and what each folder last agreed with its server is in `%LOCALAPPDATA%\Accordion\state`.
- **Removing an account** signs this computer out of it. Files you kept on the computer stay, and placeholders whose contents were never downloaded go.

## Crates

| Crate | What it is |
|---|---|
| `crates/accordion-desktop` | The Tauri app: the tray icon, the window in `ui/`, accounts, and a sync loop for each account. |
| `crates/cloud-files` | Windows' Cloud Files API, through the `windows` crate: sync roots, placeholders, and sending a file's contents when something opens it. |

The engine decides what goes where. On Windows, the app gives it an `OnDemand` (`src/files.rs`), so files arrive as placeholders instead of downloading.

## Building

The app builds on Windows with the Visual Studio Build Tools and Rust's MSVC toolchain. WebView2 comes with Windows 10 and 11.

```powershell
cargo build -p accordion-desktop
cargo clippy --workspace --all-targets
cargo test --workspace
```

To build the installer, install the Tauri CLI, then build from the app's folder. The installer is in `target\release\bundle\nsis`.

```powershell
cargo install --locked tauri-cli --version 2.12.4
cd crates\accordion-desktop
cargo tauri build
```

On Linux, Tauri needs GTK and WebKit: `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`, and `libxdo-dev`.

### Trying a build

Set `ACCORDION_DESKTOP_HOME` to a folder to keep a build's list of accounts and sync state apart from your own. Its sign-ins still go in Credential Manager, under a new name for each account. A build started that way doesn't turn on starting at sign-in.

`crates/cloud-files/examples/try-root.rs` tries the Cloud Files calls on their own, with made-up files:

```powershell
cargo run -p cloud-files --example try-root -- register C:\path\to\folder
cargo run -p cloud-files --example try-root -- selftest C:\path\to\folder
cargo run -p cloud-files --example try-root -- unregister
```

## Releases

Pushing a version tag builds the Windows installer on GitHub's runner and drafts a pre-release with it (`.github/workflows/release.yml`). The first alpha isn't signed, so Windows warns before it runs. [Decision 24](https://github.com/accordion-suite/accordion/blob/main/docs/decisions/0024-signing-the-apps.md) explains how signing comes next.

## License

[AGPL-3.0 or later](LICENSE)
