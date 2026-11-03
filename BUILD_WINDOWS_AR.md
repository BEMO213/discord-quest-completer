# Windows build

The workflow `.github/workflows/build-hardened-windows.yml` builds the current Windows runner from `src-win`, runs the Rust security tests, and packages the Tauri desktop application as NSIS EXE and MSI installers.

After a successful GitHub Actions run, download the artifact named `discord-quest-completer-windows-installer`.

Security changes in this branch:
- Reject path traversal, absolute paths, invalid Windows path components, and reserved device names.
- Resolve game directories beneath the app-managed games folder and reject symlinks and Windows reparse points.
- Create dummy executables exclusively and never overwrite a different existing file.
- Track child processes started by the app and stop only their owned process handles/PIDs.
- Limit concurrent processes and downloaded game-list size, with request timeouts and same-host HTTPS redirects.
- Validate Discord activity data and return connection errors instead of panicking.
- Update Tauri, Vite, and HTTP/2 dependencies; keep Vite development server loopback-only by default.

Runtime verification in Windows Sandbox is still recommended before installing on a primary machine.
