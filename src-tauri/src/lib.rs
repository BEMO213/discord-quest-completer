// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
use once_cell::sync::OnceCell;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{path::BaseDirectory, AppHandle, Emitter, Listener, Manager, State};

mod rpc;
mod runner;

// Global static instance of the Discord client
static DISCORD_CLIENT: OnceCell<Mutex<Option<rpc::Client>>> = OnceCell::new();

#[derive(Default)]
struct ProcessRegistry {
    children: Mutex<HashMap<u32, std::process::Child>>,
}

impl Drop for ProcessRegistry {
    fn drop(&mut self) {
        if let Ok(children) = self.children.get_mut() {
            for child in children.values_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

const MAX_GAME_LIST_BYTES: usize = 20 * 1024 * 1024;
const MAX_GAME_ENTRIES: usize = 100_000;
const MAX_MANAGED_PROCESSES: usize = 32;

fn get_discord_client() -> &'static Mutex<Option<rpc::Client>> {
    DISCORD_CLIENT.get_or_init(|| Mutex::new(None))
}

fn runner_resource_name() -> &'static str {
    #[cfg(target_os = "windows")]
    let runner_name = "data/src-win.exe";

    #[cfg(target_os = "linux")]
    let runner_name = "data/src-linux";

    #[cfg(target_os = "macos")]
    let runner_name = "data/src-darwin";

    runner_name
}

fn is_app_bundle(executable_name: &str) -> bool {
    cfg!(target_os = "macos") && executable_name.ends_with(".app")
}

fn bundle_binary_name(app_name: &str) -> String {
    app_name
        .strip_suffix(".app")
        .unwrap_or(app_name)
        .to_string()
}

fn validate_path_component(value: &str) -> Result<(), String> {
    if value.is_empty() || value == "." || value == ".." {
        return Err("Empty, current-directory, and parent-directory components are not allowed".into());
    }
    if value.encode_utf16().count() > 255 {
        return Err("A path component exceeds the filename length limit".into());
    }
    if value.chars().any(|ch| {
        ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\')
    }) {
        return Err(format!("Invalid character in path component: {value:?}"));
    }
    if value.ends_with('.') || value.ends_with(' ') {
        return Err("Windows path components cannot end with a dot or space".into());
    }

    let stem = value.split('.').next().unwrap_or(value)
        .trim_end_matches(|ch| ch == ' ' || ch == '.')
        .to_ascii_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$" |
        "COM¹" | "COM²" | "COM³" | "LPT¹" | "LPT²" | "LPT³"
    ) || (1..=9).any(|n| stem == format!("COM{n}") || stem == format!("LPT{n}"));
    if reserved {
        return Err(format!("Reserved Windows device name is not allowed: {value:?}"));
    }
    Ok(())
}

fn validate_relative_game_path(path: &str) -> Result<Vec<String>, String> {
    if path.len() > 1024 {
        return Err("Game executable path is too long".into());
    }
    if path.is_empty() {
        return Ok(Vec::new());
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err("Absolute and network paths are not allowed".into());
    }

    let mut components = Vec::new();
    for component in path.split(|ch| ch == '/' || ch == '\\') {
        validate_path_component(component)?;
        components.push(component.to_owned());
        if components.len() > 32 {
            return Err("Game path contains too many directory levels".into());
        }
    }
    Ok(components)
}

fn validate_executable_name(name: &str) -> Result<(), String> {
    validate_path_component(name)?;
    if name.contains('/') || name.contains('\\') {
        return Err("Executable name must be a single filename".into());
    }
    Ok(())
}

fn validate_app_id(app_id: &str) -> Result<String, String> {
    let id = app_id.parse::<u64>()
        .map_err(|_| "Invalid Discord application ID".to_string())?;
    if id == 0 || id > i64::MAX as u64 {
        return Err("Discord application ID is outside the supported range".into());
    }
    Ok(id.to_string())
}

fn is_link_or_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

fn ensure_safe_directory(
    parent: &Path,
    name: &str,
    allowed_root: &Path,
    create: bool,
) -> Result<PathBuf, String> {
    validate_path_component(name)?;
    let candidate = parent.join(name);
    if create {
        match fs::create_dir(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("Failed to create directory {:?}: {error}", candidate)),
        }
    }

    let metadata = fs::symlink_metadata(&candidate)
        .map_err(|e| format!("Directory unavailable at {:?}: {e}", candidate))?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
        return Err(format!("Expected a regular directory, not a link or file: {:?}", candidate));
    }

    let canonical = fs::canonicalize(&candidate)
        .map_err(|e| format!("Failed to resolve directory {:?}: {e}", candidate))?;
    if !canonical.starts_with(allowed_root) {
        return Err(format!("Directory escaped its allowed parent: {:?}", candidate));
    }
    Ok(canonical)
}

fn game_folder_path(
    exe_dir: &Path,
    relative_path: &str,
    app_id: &str,
    create: bool,
) -> Result<PathBuf, String> {
    let app_id = validate_app_id(app_id)?;
    let components = validate_relative_game_path(relative_path)?;
    let root = fs::canonicalize(exe_dir)
        .map_err(|e| format!("Failed to resolve application directory: {e}"))?;

    let games_dir = ensure_safe_directory(&root, "games", &root, create)?;
    let app_dir = ensure_safe_directory(&games_dir, &app_id, &games_dir, create)?;
    let mut current = app_dir.clone();
    for component in components {
        current = ensure_safe_directory(&current, &component, &app_dir, create)?;
    }
    Ok(current)
}

fn ensure_regular_file_within(path: &Path, allowed_root: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("File unavailable at {:?}: {e}", path))?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
        return Err(format!("Expected a regular file, not a link: {:?}", path));
    }
    let canonical = fs::canonicalize(path)
        .map_err(|e| format!("Failed to resolve file {:?}: {e}", path))?;
    if !canonical.starts_with(allowed_root) {
        return Err(format!("File escaped its allowed directory: {:?}", path));
    }
    Ok(canonical)
}

fn copy_runner_without_overwrite(
    runner_template: &Path,
    destination: &Path,
    allowed_root: &Path,
) -> Result<PathBuf, String> {
    let template_metadata = fs::symlink_metadata(runner_template)
        .map_err(|e| format!("Failed to inspect runner template: {e}"))?;
    if is_link_or_reparse_point(&template_metadata)
        || !template_metadata.is_file()
        || template_metadata.len() == 0
        || template_metadata.len() > 50 * 1024 * 1024
    {
        return Err("Bundled runner template is not a valid regular file".into());
    }
    let bytes = fs::read(runner_template)
        .map_err(|e| format!("Failed to read runner template: {e}"))?;

    match fs::OpenOptions::new().write(true).create_new(true).open(destination) {
        Ok(mut output) => {
            if let Err(error) = output.write_all(&bytes).and_then(|_| output.flush()) {
                drop(output);
                let _ = fs::remove_file(destination);
                return Err(format!("Failed to write dummy executable: {error}"));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing_path = ensure_regular_file_within(destination, allowed_root)?;
            let existing = fs::read(&existing_path)
                .map_err(|e| format!("Failed to read existing executable: {e}"))?;
            if existing != bytes {
                return Err("A different file already exists; it was not overwritten".into());
            }
            return Ok(existing_path);
        }
        Err(error) => return Err(format!("Failed to create dummy executable: {error}")),
    }

    ensure_regular_file_within(destination, allowed_root)
}

fn xml_escape(value: &str) -> String {
    value.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn resolve_runner_template(handle: &AppHandle) -> Result<PathBuf, String> {
    handle
        .path()
        .resolve(runner_resource_name(), BaseDirectory::Resource)
        .map_err(|e| format!("Failed to resolve runner template: {}", e))
}

fn make_macos_app_bundle(
    app_bundle_path: &Path,
    app_name: &str,
    display_name: &str,
    runner_template: &Path,
) -> Result<PathBuf, String> {
    let binary_name = bundle_binary_name(app_name);
    let macos_dir = app_bundle_path.join("Contents/MacOS");
    fs::create_dir_all(&macos_dir)
        .map_err(|e| format!("Failed to create app bundle directories: {}", e))?;

    let target_executable_path = macos_dir.join(&binary_name);
    fs::copy(runner_template, &target_executable_path)
        .map_err(|e| format!("Failed to copy dummy executable: {}", e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&target_executable_path)
            .map_err(|e| format!("Failed to read executable permissions: {}", e))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&target_executable_path, permissions)
            .map_err(|e| format!("Failed to set executable permissions: {}", e))?;
    }

    let info_plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDisplayName</key>
    <string>{display_name}</string>
    <key>CFBundleExecutable</key>
    <string>{binary_name}</string>
    <key>CFBundleIdentifier</key>
    <string>me.markterence.discordquestcompleter.dummy</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>
"#
    );

    fs::write(app_bundle_path.join("Contents/Info.plist"), info_plist)
        .map_err(|e| format!("Failed to write Info.plist: {}", e))?;

    Ok(target_executable_path)
}

fn launch_executable_path(game_folder_path: &Path, executable_name: &str) -> PathBuf {
    if is_app_bundle(executable_name) {
        game_folder_path
            .join(executable_name)
            .join("Contents/MacOS")
            .join(bundle_binary_name(executable_name))
    } else {
        game_folder_path.join(executable_name)
    }
}

fn stop_process_name(exec_name: &str) -> String {
    if is_app_bundle(exec_name) {
        bundle_binary_name(exec_name)
    } else {
        exec_name.to_string()
    }
}

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[tauri::command(rename_all = "snake_case")]
async fn create_fake_game(
    handle: AppHandle,
    path: &str,
    executable_name: &str,
    app_id: &str,
    display_name: Option<String>,
) -> Result<String, String> {
    validate_executable_name(executable_name)?;

    let executable_path = env::current_exe()
        .map_err(|e| format!("Failed to locate application executable: {e}"))?;
    let executable_dir = executable_path.parent()
        .ok_or_else(|| "Application executable has no parent directory".to_string())?;
    let game_dir = game_folder_path(executable_dir, path, app_id, true)?;
    let runner_template = resolve_runner_template(&handle)?;

    if is_app_bundle(executable_name) {
        let bundle_dir = ensure_safe_directory(&game_dir, executable_name, &game_dir, true)?;
        let contents_dir = ensure_safe_directory(&bundle_dir, "Contents", &bundle_dir, true)?;
        let macos_dir = ensure_safe_directory(&contents_dir, "MacOS", &bundle_dir, true)?;
        let binary_name = bundle_binary_name(executable_name);
        let copied = copy_runner_without_overwrite(
            &runner_template,
            &macos_dir.join(&binary_name),
            &bundle_dir,
        )?;

        let title = display_name
            .filter(|value| !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control))
            .unwrap_or_else(|| binary_name.clone());
        let plist_path = contents_dir.join("Info.plist");
        let safe_title = xml_escape(&title);
        let safe_binary = xml_escape(&binary_name);
        let plist_content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDisplayName</key><string>{safe_title}</string>
    <key>CFBundleExecutable</key><string>{safe_binary}</string>
    <key>CFBundleIdentifier</key><string>me.markterence.discordquestcompleter.dummy</string>
    <key>CFBundleName</key><string>{safe_title}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>1.0</string>
    <key>CFBundleVersion</key><string>1</string>
</dict>
</plist>
"#
        );
        match fs::OpenOptions::new().write(true).create_new(true).open(&plist_path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(plist_content.as_bytes()) {
                    drop(file);
                    let _ = fs::remove_file(&plist_path);
                    return Err(format!("Failed to write app bundle Info.plist: {error}"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(&plist_path)
                    .map_err(|e| format!("Failed to inspect existing Info.plist: {e}"))?;
                if is_link_or_reparse_point(&meta) || !meta.is_file() {
                    return Err("Existing Info.plist is not a regular file".into());
                }
            }
            Err(error) => return Err(format!("Failed to create app bundle Info.plist: {error}")),
        }
        return Ok(format!("Dummy app bundle created at: {:?}", copied));
    }

    let copied = copy_runner_without_overwrite(
        &runner_template,
        &game_dir.join(executable_name),
        &game_dir,
    )?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&copied)
            .map_err(|e| format!("Failed to read executable permissions: {e}"))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&copied, permissions)
            .map_err(|e| format!("Failed to set executable permissions: {e}"))?;
    }

    Ok(format!("Dummy executable copied to: {:?}", copied))
}

fn launch_executable_path(game_dir: &Path, executable_name: &str) -> PathBuf {
    if is_app_bundle(executable_name) {
        game_dir.join(executable_name)
            .join("Contents/MacOS")
            .join(bundle_binary_name(executable_name))
    } else {
        game_dir.join(executable_name)
    }
}

#[tauri::command(rename_all = "snake_case")]
async fn run_background_process(
    handle: AppHandle,
    process_registry: State<'_, ProcessRegistry>,
    name: &str,
    path: &str,
    executable_name: &str,
    app_id: &str,
) -> Result<u32, String> {
    validate_executable_name(executable_name)?;
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err("Game title is empty, invalid, or too long".into());
    }

    let application_path = env::current_exe()
        .map_err(|e| format!("Failed to locate application executable: {e}"))?;
    let application_dir = application_path.parent()
        .ok_or_else(|| "Application executable has no parent directory".to_string())?;
    let game_dir = game_folder_path(application_dir, path, app_id, false)?;
    let executable = ensure_regular_file_within(
        &launch_executable_path(&game_dir, executable_name),
        &game_dir,
    )?;

    // Only launch the exact runner binary bundled with this application.
    let runner_template = resolve_runner_template(&handle)?;
    let runner_metadata = fs::symlink_metadata(&runner_template)
        .map_err(|e| format!("Failed to inspect bundled runner: {e}"))?;
    if is_link_or_reparse_point(&runner_metadata)
        || !runner_metadata.is_file()
        || runner_metadata.len() == 0
        || runner_metadata.len() > 50 * 1024 * 1024
    {
        return Err("Bundled runner is not a valid regular file".into());
    }
    let executable_metadata = fs::metadata(&executable)
        .map_err(|e| format!("Failed to inspect selected executable: {e}"))?;
    if executable_metadata.len() != runner_metadata.len() {
        return Err("Selected executable does not match the bundled runner".into());
    }
    let runner_bytes = fs::read(&runner_template)
        .map_err(|e| format!("Failed to read bundled runner: {e}"))?;
    let selected_bytes = fs::read(&executable)
        .map_err(|e| format!("Failed to read selected executable: {e}"))?;
    if selected_bytes != runner_bytes {
        return Err("Selected executable does not match the bundled runner".into());
    }

    let mut children = process_registry.children.lock()
        .map_err(|_| "Process registry is unavailable".to_string())?;
    children.retain(|_, child| child.try_wait().map(|status| status.is_none()).unwrap_or(true));
    if children.len() >= MAX_MANAGED_PROCESSES {
        return Err(format!("At most {MAX_MANAGED_PROCESSES} processes can be managed at once"));
    }

    let mut command = std::process::Command::new(&executable);
    command.arg("--title").arg(name).current_dir(&game_dir);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let child = command.spawn()
        .map_err(|e| format!("Failed to start executable: {e}"))?;
    let process_id = child.id();
    children.insert(process_id, child);
    Ok(process_id)
}

#[tauri::command(rename_all = "snake_case")]
async fn stop_process(
    process_id: u32,
    process_registry: State<'_, ProcessRegistry>,
) -> Result<(), String> {
    let mut child = {
        let mut children = process_registry.children.lock()
            .map_err(|_| "Process registry is unavailable".to_string())?;
        children.remove(&process_id)
            .ok_or_else(|| "This process was not started by this app session".to_string())?
    };

    match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(error) => {
            let mut children = process_registry.children.lock()
                .map_err(|_| "Process registry is unavailable".to_string())?;
            children.insert(process_id, child);
            return Err(format!("Failed to inspect process {process_id}: {error}"));
        }
    }

    if let Err(kill_error) = child.kill() {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {
                let mut children = process_registry.children.lock()
                    .map_err(|_| "Process registry is unavailable".to_string())?;
                children.insert(process_id, child);
                return Err(format!("Failed to stop process {process_id}: {kill_error}"));
            }
            Err(check_error) => {
                let mut children = process_registry.children.lock()
                    .map_err(|_| "Process registry is unavailable".to_string())?;
                children.insert(process_id, child);
                return Err(format!("Failed to stop process {process_id}: {kill_error}; process state: {check_error}"));
            }
        }
    }

    child.wait()
        .map(|_| ())
        .map_err(|e| format!("Failed to wait for process {process_id}: {e}"))
}

/// Usage: Calling from JS:
/// ```javascript
/// await invoke('connect_to_discord_rpc_3', json, 'connect' | 'disconnect');
#[tauri::command(rename_all = "snake_case")]
async fn connect_to_discord_rpc_3(
    handle: AppHandle,
    activity_json: String,
    action: String,
) -> Result<(), String> {
    match action.as_str() {
        "disconnect" => {
            let previous = {
                let mut guard = get_discord_client().lock()
                    .map_err(|_| "Discord client registry is unavailable".to_string())?;
                guard.take()
            };
            if let Some(client) = previous {
                client.discord.disconnect().await;
            }
            handle.emit("client_disconnected", ())
                .map_err(|e| format!("Failed to notify frontend about disconnect: {e}"))?;
            Ok(())
        }
        "connect" => {
            let activity = runner::parse_activity_json(&activity_json)?;
            let app_id = activity.app_id.clone();
            let previous = {
                let mut guard = get_discord_client().lock()
                    .map_err(|_| "Discord client registry is unavailable".to_string())?;
                guard.take()
            };
            if let Some(client) = previous {
                client.discord.disconnect().await;
            }

            handle.emit("client_connecting", serde_json::json!({"app_id": app_id}))
                .map_err(|e| format!("Failed to notify frontend about Discord connection: {e}"))?;
            let client = runner::set_activity(activity_json).await?;
            {
                let mut guard = get_discord_client().lock()
                    .map_err(|_| "Discord client registry is unavailable".to_string())?;
                *guard = Some(client);
            }
            if let Err(error) = handle.emit("client_connected", serde_json::json!({"app_id": app_id})) {
                eprintln!("Discord connected but frontend notification failed: {error}");
            }
            Ok(())
        }
        _ => Err("Unsupported Discord RPC action".into()),
    }
}

async fn fetch_gamelist(url: &'static str, allowed_host: &'static str) -> Result<serde_json::Value, String> {
    use reqwest::{redirect::Policy, Client};

    let redirect_policy = Policy::custom(move |attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects");
        }
        if attempt.url().scheme() == "https"
            && attempt.url().host_str() == Some(allowed_host)
            && attempt.url().port_or_known_default() == Some(443)
        {
            attempt.follow()
        } else {
            attempt.error("redirect to an untrusted destination was blocked")
        }
    });

    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(10))
        .redirect(redirect_policy)
        .build()
        .map_err(|e| format!("Failed to configure game-list request: {e}"))?;

    let mut response = client.get(url).send().await
        .map_err(|e| format!("Failed to download game list: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Game-list server returned HTTP {}", response.status()));
    }
    if response.content_length().is_some_and(|length| length > MAX_GAME_LIST_BYTES as u64) {
        return Err("Game list exceeds the 20 MB safety limit".into());
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await
        .map_err(|e| format!("Failed while reading game list: {e}"))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_GAME_LIST_BYTES {
            return Err("Game list exceeds the 20 MB safety limit".into());
        }
        body.extend_from_slice(&chunk);
    }

    let parsed: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| format!("Game-list response is not valid JSON: {e}"))?;
    let games = parsed.as_array()
        .ok_or_else(|| "Game-list response must be a JSON array".to_string())?;
    if games.len() > MAX_GAME_ENTRIES {
        return Err("Game list contains too many entries".into());
    }
    Ok(parsed)
}

#[tauri::command(rename_all = "snake_case")]
async fn fetch_gamelist_gh_mirror() -> Result<serde_json::Value, String> {
    fetch_gamelist(
        "https://markterence.github.io/discord-quest-completer/detectable.json",
        "markterence.github.io",
    ).await
}

#[tauri::command(rename_all = "snake_case")]
async fn fetch_gamelist_from_discord() -> Result<serde_json::Value, String> {
    fetch_gamelist("https://discord.com/api/applications/detectable", "discord.com").await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(ProcessRegistry::default())
        .setup(|app| {
            let app_handle = app.handle().clone();
            app_handle.listen("event_disconnect", move |_| {
                tauri::async_runtime::spawn(async move {
                    let previous = get_discord_client().lock().ok().and_then(|mut guard| guard.take());
                    if let Some(client) = previous {
                        client.discord.disconnect().await;
                    }
                });
            });
            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            greet,
            create_fake_game,
            stop_process,
            connect_to_discord_rpc_3,
            run_background_process,
            fetch_gamelist_gh_mirror,
            fetch_gamelist_from_discord
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod security_tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        assert!(validate_relative_game_path("../outside").is_err());
        assert!(validate_relative_game_path("..\\outside").is_err());
        assert!(validate_relative_game_path("/tmp/outside").is_err());
        assert!(validate_relative_game_path("\\\\server\\share").is_err());
        assert!(validate_relative_game_path("C:\\Windows").is_err());
    }

    #[test]
    fn rejects_reserved_and_invalid_names() {
        assert!(validate_path_component("CON").is_err());
        assert!(validate_path_component("NUL.txt").is_err());
        assert!(validate_path_component("bad?.exe").is_err());
        assert!(validate_executable_name("folder\\app.exe").is_err());
    }

    #[test]
    fn accepts_normal_relative_paths() {
        assert!(validate_relative_game_path("").is_ok());
        assert!(validate_relative_game_path("Binaries/Win64").is_ok());
        assert!(validate_executable_name("game.exe").is_ok());
    }

    #[test]
    fn validates_ids_without_javascript_number_rounding() {
        assert!(validate_app_id("0").is_err());
        assert!(validate_app_id("-1").is_err());
        assert!(validate_app_id("not-an-id").is_err());
        assert!(validate_app_id("123456789012345678").is_ok());
    }
}
