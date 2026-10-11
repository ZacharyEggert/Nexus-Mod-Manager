#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use nmm_core::download::{self, Download, Downloads};
use nmm_core::fomod::{Eval, Info, ModuleConfig, Selection};
use nmm_core::invalidation::ArchiveInvalidation;
use nmm_core::launch as launcher;
use nmm_core::manager::{ImportReport, LinkMode, Manager, ModEntry, PluginRow, Staged};
use nmm_core::modscript::{Answer, Prompt};
use nmm_core::nexus::{self, sso, User};
use nmm_core::plugins::Plugin;
use nmm_core::{discover, game, IoContext, Task};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager as _, State};
use tauri_plugin_dialog::DialogExt;

type R<T> = nmm_core::Result<T>;

/// `config.json` in the app config dir: app settings + per-game paths.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    settings: Settings,
    games: BTreeMap<String, GameConfig>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    /// Where newly set-up games keep their mods (`<root>/<game>`); `None` = app data dir.
    mods_root: Option<PathBuf>,
    /// Link mode newly set-up games start with.
    link_mode: LinkMode,
    nexus_api: nmm_core::nexus::Api,
    /// Quit after launching a game.
    close_on_launch: bool,
    /// Downloads running at once.
    max_downloads: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mods_root: None,
            link_mode: LinkMode::default(),
            nexus_api: nexus::Api::default(),
            close_on_launch: false,
            max_downloads: 3,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct GameConfig {
    game_path: PathBuf,
    mods_dir: PathBuf,
    /// Custom launch command (e.g. SMAPI); `None` = the definition's executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch: Option<String>,
    /// Folder of the game's required tool (Dark Souls 2: GeDoSaTo), `{ToolFolder}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_folder: Option<PathBuf>,
    /// Arguments for the custom launch command (C# `CustomLaunchCommandArguments`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch_args: Option<String>,
    /// Supported tool settings key -> the folder the user picked for it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tools: BTreeMap<String, PathBuf>,
}

#[derive(Default)]
struct App {
    /// Per-game lock: background tasks on one game run one at a time, other games run in parallel.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Task id -> cancel flag, while the task runs.
    tasks: Mutex<HashMap<String, Arc<AtomicBool>>>,
    staged: Mutex<HashMap<String, (String, Staged)>>,
    /// Serialises read-modify-write of `config.json`.
    config: Mutex<()>,
    /// Quit requested while tasks ran: they were cancelled; exit once the last one has rolled back.
    quitting: AtomicBool,
    /// Nexus client for the stored key (keeps rate-limit state); dropped when key or API setting changes.
    nexus: Mutex<Option<Arc<nexus::Client>>>,
    /// Account the stored key belongs to, once validated.
    user: Mutex<Option<User>>,
    /// Nexus links received, until the UI takes them.
    links: Mutex<Vec<LinkEvent>>,
    /// Download queue, opened in setup.
    downloads: OnceLock<Downloads>,
    /// Recent link texts and when they arrived; a link delivered twice (argv + OS event) counts once.
    recent_links: Mutex<Vec<(String, Instant)>>,
}

#[derive(Clone, Serialize)]
struct LinkEvent {
    link: nexus::link::Link,
    /// Game modes the link's domain belongs to; the UI picks a configured one.
    modes: Vec<String>,
}

/// Queue `text` if it's a Nexus link and tell the UI.
fn receive_link(app: &AppHandle, text: &str) {
    let Some(link) = nexus::link::parse(text) else {
        return;
    };
    let st = app.state::<App>();
    {
        let mut recent = st.recent_links.lock().unwrap();
        recent.retain(|(_, t)| t.elapsed() < Duration::from_secs(5));
        if recent.iter().any(|(s, _)| s == text) {
            return;
        }
        recent.push((text.into(), Instant::now()));
    }
    tracing::info!(domain = %link.domain, mod_id = link.mod_id, file_id = ?link.file_id, "Nexus link received");
    let modes = game::modes_for_domain(&link.domain);
    st.links.lock().unwrap().push(LinkEvent { link, modes });
    let _ = app.emit("nxm", ());
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Links received so far (each returned once).
#[tauri::command]
fn take_links(st: State<'_, App>) -> Vec<LinkEvent> {
    std::mem::take(&mut *st.links.lock().unwrap())
}

impl App {
    /// Cancel running tasks and return true when quitting must wait for their rollback.
    fn hold_quit(&self, app: &AppHandle) -> bool {
        let tasks = self.tasks.lock().unwrap();
        if tasks.is_empty() {
            return false;
        }
        tasks
            .values()
            .for_each(|c| c.store(true, Ordering::Relaxed));
        self.quitting.store(true, Ordering::Relaxed);
        let _ = app.emit("quitting", ());
        true
    }

    fn lock(&self, mode: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(mode.into())
            .or_default()
            .clone()
    }
}

#[derive(Clone, Serialize)]
struct Progress<'a> {
    id: &'a str,
    step: &'a str,
    done: u64,
    total: u64,
}

#[derive(Clone, Serialize)]
struct Outcome<T> {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Run `f` on a worker thread holding `mode`'s lock. Emits `progress` while it runs, then `done`
/// (with the result) or `error`, all tagged with the caller-chosen `id`; `cancel(id)` stops it.
/// `what` names the task in the log.
fn spawn<T: Serialize + Clone + 'static>(
    app: &AppHandle,
    id: String,
    mode: String,
    what: String,
    f: impl FnOnce(&AppHandle, Task) -> R<T> + Send + 'static,
) {
    let st = app.state::<App>();
    let task = Task::default();
    st.tasks
        .lock()
        .unwrap()
        .insert(id.clone(), task.cancel.clone());
    let lock = st.lock(&mode);
    let (app, emit_id) = (app.clone(), id.clone());
    let emitter = app.clone();
    let last = Mutex::new(Instant::now() - Duration::from_secs(1));
    let progress = move |step: &str, done: u64, total: u64| {
        // ponytail: ~20 events/s cap; plenty for a progress bar.
        let mut l = last.lock().unwrap();
        if l.elapsed() >= Duration::from_millis(50) {
            *l = Instant::now();
            let _ = emitter.emit(
                "progress",
                Progress {
                    id: &emit_id,
                    step,
                    done,
                    total,
                },
            );
        }
    };
    let task = Task {
        progress: Arc::new(progress),
        ..task
    };
    std::thread::spawn(move || {
        let res = {
            let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
            let start = Instant::now();
            tracing::info!(task = %id, game = %mode, "{what}: started");
            // The panic itself is logged by the hook installed in main.
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&app, task)))
                .unwrap_or_else(|_| Err("internal error (panic); see the log".into()));
            let ms = start.elapsed().as_millis();
            match &res {
                Ok(_) => tracing::info!(task = %id, ms, "{what}: done"),
                Err(nmm_core::Error::Cancelled) => {
                    tracing::info!(task = %id, ms, "{what}: cancelled")
                }
                Err(e) => tracing::error!(task = %id, ms, "{what}: {e}"),
            }
            res
        };
        let st = app.state::<App>();
        let last = {
            let mut tasks = st.tasks.lock().unwrap();
            tasks.remove(&id);
            tasks.is_empty()
        };
        let _ = match res {
            Ok(v) => app.emit(
                "done",
                Outcome {
                    id,
                    result: Some(v),
                    error: None,
                },
            ),
            Err(e) => app.emit(
                "error",
                Outcome::<()> {
                    id,
                    result: None,
                    error: Some(e.to_string()),
                },
            ),
        };
        if last && st.quitting.load(Ordering::Relaxed) {
            app.exit(0);
        }
    });
}

#[tauri::command]
fn cancel(st: State<'_, App>, id: String) {
    if let Some(c) = st.tasks.lock().unwrap().get(&id) {
        c.store(true, Ordering::Relaxed);
    }
}

fn config_path(app: &AppHandle) -> R<PathBuf> {
    Ok(app
        .path()
        .app_config_dir()
        .map_err(|e| e.to_string())?
        .join("config.json"))
}

fn load_config(app: &AppHandle) -> Config {
    let Ok(path) = config_path(app) else {
        return Config::default();
    };
    let Ok(s) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    serde_json::from_str(&s).unwrap_or_else(|e| {
        tracing::error!("{}: {e}; using defaults", path.display());
        Config::default()
    })
}

/// Read-modify-write `config.json`, durably (a torn write would forget every game's paths).
fn update_config(app: &AppHandle, f: impl FnOnce(&mut Config)) -> R<()> {
    let st = app.state::<App>();
    let _g = st.config.lock().unwrap();
    let mut cfg = load_config(app);
    f(&mut cfg);
    let path = config_path(app)?;
    std::fs::create_dir_all(path.parent().unwrap()).at(path.parent().unwrap())?;
    nmm_core::write_durable(&path, &serde_json::to_vec_pretty(&cfg).unwrap())
}

fn default_mods_dir(app: &AppHandle, mode: &str) -> R<PathBuf> {
    let root = match load_config(app).settings.mods_root {
        Some(r) => r,
        None => app
            .path()
            .app_data_dir()
            .map_err(|e| e.to_string())?
            .join("Mods"),
    };
    Ok(root.join(mode))
}

/// Open a game's manager. Callers that write must hold the game's lock (i.e. run inside `spawn`).
/// Refused while the game misses a critical file or its required tool (C# start-up check).
fn manager(app: &AppHandle, mode: &str) -> R<Manager> {
    let m = open_manager(app, mode)?;
    match m.game.missing_files(&m.game_path) {
        Some(why) => Err(why.into()),
        None => Ok(m),
    }
}

fn open_manager(app: &AppHandle, mode: &str) -> R<Manager> {
    let mut def = game::find(mode).ok_or("unknown game")?;
    let cfg = load_config(app)
        .games
        .get(mode)
        .cloned()
        .ok_or("game path not set")?;
    def.tool_folder = cfg.tool_folder;
    // Heroic / Lutris prefixes live outside the game folder; Proton and in-prefix installs are
    // found from the path itself.
    if cfg!(not(windows)) && def.windows_user_dir(&cfg.game_path).is_none() {
        def.user_dir = discover::wine_user_dir(&cfg.game_path);
    }
    Manager::open(def, cfg.game_path, cfg.mods_dir)
}

/// Read-only view for commands that don't mutate. Waits out a running task, since `open`
/// may recover a deploy journal; the UI only reads between tasks.
fn read(app: &AppHandle, mode: &str) -> R<Manager> {
    let lock = app.state::<App>().lock(mode);
    let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
    manager(app, mode)
}

#[derive(Serialize)]
struct GameInfo {
    mode_id: String,
    name: String,
    game_path: Option<PathBuf>,
    mods_dir: Option<PathBuf>,
    launch: Option<String>,
    /// What Launch runs when `launch` is unset.
    default_launch: Option<String>,
    detected: Option<PathBuf>,
    /// Tool whose folder setup asks for (`GeDoSaTo`).
    required_tool: Option<String>,
    tool_folder: Option<PathBuf>,
    launch_args: Option<String>,
    /// Whether a custom launch command may be set.
    allow_custom: bool,
    /// Other games that share this one's downloads (SkyrimSE <-> SkyrimGOG).
    shares_with: Vec<String>,
}

#[tauri::command]
async fn games(app: AppHandle) -> Vec<GameInfo> {
    let cfg = load_config(&app);
    let env = discover::Env::system();
    let mut v: Vec<GameInfo> = game::all()
        .into_iter()
        .map(|g| {
            let c = cfg.games.get(&g.mode_id);
            GameInfo {
                // Configured games aren't detected again (setup shows the saved path).
                detected: c.is_none().then(|| discover::detect_in(&g, &env)).flatten(),
                game_path: c.map(|c| c.game_path.clone()),
                mods_dir: c.map(|c| c.mods_dir.clone()),
                launch: c.and_then(|c| c.launch.clone()),
                default_launch: g.default_executable().map(String::from),
                tool_folder: c.and_then(|c| c.tool_folder.clone()),
                launch_args: c.and_then(|c| c.launch_args.clone()),
                allow_custom: g.allows_custom_command(),
                shares_with: game::storage_group(&g.mode_id)
                    .into_iter()
                    .filter(|m| m != &g.mode_id)
                    .collect(),
                required_tool: g.required_tool_name.clone(),
                mode_id: g.mode_id,
                name: g.name,
            }
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

#[tauri::command]
async fn configure(
    app: AppHandle,
    mode: String,
    game_path: PathBuf,
    mods_dir: Option<PathBuf>,
    launch: Option<String>,
    tool_folder: Option<PathBuf>,
    launch_args: Option<String>,
) -> R<()> {
    if !game_path.is_dir() {
        return Err(format!("{} is not a folder", game_path.display()).into());
    }
    let mods_dir = match mods_dir {
        Some(d) => d,
        None => default_mods_dir(&app, &mode)?,
    };
    let fresh = !mods_dir.join("state.json").exists();
    tracing::info!(game = %mode, game_path = %game_path.display(), mods_dir = %mods_dir.display(), "configured");
    update_config(&app, |c| {
        // Tool folders picked earlier survive re-running setup.
        let tools = c
            .games
            .get(&mode)
            .map(|g| g.tools.clone())
            .unwrap_or_default();
        c.games.insert(
            mode.clone(),
            GameConfig {
                game_path,
                mods_dir,
                launch: launch.filter(|l| !l.trim().is_empty()),
                tool_folder: tool_folder.filter(|t| !t.as_os_str().is_empty()),
                launch_args: launch_args.filter(|l| !l.trim().is_empty()),
                tools,
            },
        );
    })?;
    if fresh {
        // A new mods folder starts with the default link mode; nothing is deployed yet, so no relink.
        let lock = app.state::<App>().lock(&mode);
        let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = open_manager(&app, &mode)?;
        m.state.link_mode = load_config(&app).settings.link_mode;
        m.state.deployed_mode = m.state.link_mode;
        m.save()?;
    }
    Ok(())
}

#[derive(Serialize)]
struct SettingsView {
    #[serde(flatten)]
    settings: Settings,
    log_dir: Option<PathBuf>,
}

#[tauri::command]
async fn settings(app: AppHandle) -> SettingsView {
    SettingsView {
        settings: load_config(&app).settings,
        log_dir: app.path().app_log_dir().ok(),
    }
}

#[tauri::command]
async fn save_settings(app: AppHandle, settings: Settings) -> R<()> {
    let max = settings.max_downloads;
    update_config(&app, |c| c.settings = settings)?;
    if let Some(d) = app.state::<App>().downloads.get() {
        d.set_max(max);
    }
    // API mode may have changed.
    blocking(move || {
        app.state::<App>().nexus.lock().unwrap().take();
        Ok(())
    })
    .await
}

// ---------- Nexus account ----------

/// Run on a blocking thread: reqwest's blocking client panics if built, used or dropped in async code.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> R<T> + Send + 'static) -> R<T> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| nmm_core::Error::Other(format!("internal error: {e}")))?
}

/// The API key lives in the OS keychain (Keychain / Credential Manager / Secret Service), never in config.json.
#[cfg(not(debug_assertions))]
mod key {
    use super::{AppHandle, R};
    fn entry() -> R<keyring::Entry> {
        keyring::Entry::new("llc.ex-nihilo.nexus-mod-manager", "nexus-api-key")
            .map_err(|e| format!("keychain: {e}").into())
    }
    pub fn get(_: &AppHandle) -> Option<String> {
        entry().ok()?.get_password().ok()
    }
    pub fn set(_: &AppHandle, key: &str) -> R<()> {
        entry()?
            .set_password(key)
            .map_err(|e| format!("keychain: {e}").into())
    }
    pub fn delete(_: &AppHandle) -> R<()> {
        match entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("keychain: {e}").into()),
        }
    }
}

/// Dev builds: plaintext file beside config.json, so fresh (re-signed) builds don't trigger keychain prompts.
#[cfg(debug_assertions)]
mod key {
    use super::{config_path, AppHandle, R};
    fn path(app: &AppHandle) -> R<std::path::PathBuf> {
        Ok(config_path(app)?.with_file_name("nexus-api-key.dev"))
    }
    pub fn get(app: &AppHandle) -> Option<String> {
        std::fs::read_to_string(path(app).ok()?).ok()
    }
    pub fn set(app: &AppHandle, key: &str) -> R<()> {
        let p = path(app)?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        Ok(std::fs::write(p, key).map_err(|e| e.to_string())?)
    }
    pub fn delete(app: &AppHandle) -> R<()> {
        match std::fs::remove_file(path(app)?) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string().into()),
            _ => Ok(()),
        }
    }
}

/// Nexus client for the stored key.
fn nexus_client(app: &AppHandle) -> R<Arc<nexus::Client>> {
    let st = app.state::<App>();
    let mut c = st.nexus.lock().unwrap();
    if let Some(c) = &*c {
        return Ok(c.clone());
    }
    let key = key::get(app).ok_or("not logged in to Nexus")?;
    let client = Arc::new(nexus::Client::new(
        &key,
        load_config(app).settings.nexus_api,
    ));
    *c = Some(client.clone());
    Ok(client)
}

/// Validate `key`, then store it as the account key.
fn adopt_key(app: &AppHandle, key: &str) -> R<User> {
    let key = key.trim();
    let user = match nexus::Client::new(key, nexus::Api::Rest).validate() {
        Err(nmm_core::Error::Unauthorized) => return Err("Nexus rejected this API key".into()),
        r => r?,
    };
    key::set(app, key)?;
    let st = app.state::<App>();
    *st.nexus.lock().unwrap() = None;
    *st.user.lock().unwrap() = Some(user.clone());
    tracing::info!(user = %user.name, premium = user.premium, "Nexus key stored");
    Ok(user)
}

#[derive(Serialize)]
struct Account {
    user: Option<User>,
    /// Browser login is available (this build has a Nexus application slug).
    sso: bool,
    /// Key stored but couldn't be checked (offline, rate limited, rejected).
    error: Option<String>,
}

#[tauri::command]
async fn nexus_account(app: AppHandle) -> R<Account> {
    blocking(move || {
        let st = app.state::<App>();
        let sso = sso::APP.is_some();
        if let Some(u) = st.user.lock().unwrap().clone() {
            return Ok(Account {
                user: Some(u),
                sso,
                error: None,
            });
        }
        let (user, error) = match nexus_client(&app) {
            Err(_) => (None, None), // no key stored
            Ok(c) => match c.validate() {
                Ok(u) => (Some(u), None),
                Err(e) => (None, Some(e.to_string())),
            },
        };
        *st.user.lock().unwrap() = user.clone();
        Ok(Account { user, sso, error })
    })
    .await
}

#[tauri::command]
async fn nexus_set_key(app: AppHandle, key: String) -> R<User> {
    blocking(move || adopt_key(&app, &key)).await
}

/// Task: log in through the browser (SSO); resolves with the account.
#[tauri::command]
fn nexus_login(app: AppHandle, id: String) -> R<()> {
    let slug =
        sso::APP.ok_or("this build has no Nexus application id; paste an API key instead")?;
    spawn(
        &app,
        id,
        "nexus".into(),
        "Nexus SSO login".into(),
        move |app, task| {
            use tauri_plugin_opener::OpenerExt;
            let key = sso::login(sso::URL, slug, &task, &|url| {
                if let Err(e) = app.opener().open_url(url, None::<&str>) {
                    tracing::error!("open browser: {e}; visit {url}");
                }
            })?;
            adopt_key(app, &key)
        },
    );
    Ok(())
}

#[tauri::command]
async fn nexus_logout(app: AppHandle) -> R<()> {
    blocking(move || {
        key::delete(&app)?;
        let st = app.state::<App>();
        st.nexus.lock().unwrap().take();
        st.user.lock().unwrap().take();
        tracing::info!("Nexus key removed");
        Ok(())
    })
    .await
}

#[derive(Serialize)]
struct Library {
    mods: Vec<ModEntry>,
    conflicts: BTreeMap<String, Vec<String>>,
    profiles: Vec<String>,
    install_root: PathBuf,
    link_mode: LinkMode,
    same_volume: bool,
    in_sync: bool,
    /// Stored mod folders missing from the list (state.json lost); `adopt` re-adds them.
    strays: Vec<String>,
    /// Nexus category id -> name.
    categories: BTreeMap<u64, String>,
    /// Unix seconds of the last Nexus update check.
    nexus_checked: Option<u64>,
    /// The game has a plugin load order (Plugins tab).
    uses_plugins: bool,
    /// LOOT can sort its plugins.
    loot: bool,
    /// Archive invalidation on/off; None for games that don't need it.
    archive_invalidation: Option<bool>,
    /// The game ships a base-file list, so loose files can be told apart (`loose_files`).
    has_base_files: bool,
}

#[tauri::command]
async fn library(app: AppHandle, mode: String) -> R<Library> {
    let m = read(&app, &mode)?;
    Ok(Library {
        in_sync: m.in_sync(),
        strays: m.strays(),
        conflicts: m.conflicts(),
        install_root: m.install_root(),
        link_mode: m.state.link_mode,
        same_volume: m.same_volume(),
        profiles: m.state.profiles.keys().cloned().collect(),
        categories: categories(&m).into_iter().map(|c| (c.id, c.name)).collect(),
        nexus_checked: m.state.nexus_checked,
        uses_plugins: nmm_core::plugins::Policy::new(&m.game, &m.game_path).is_some(),
        has_base_files: !m.game.base_files().is_empty(),
        loot: nmm_core::loot::supported(&m.game.mode_id),
        archive_invalidation: ArchiveInvalidation::new(&m.game, &m.game_path)
            .map(|a| a.is_active()),
        mods: m.state.mods,
    })
}

/// Whether to go on uninstalling `id`: asks the mod's warning (C# `CheckSecondaryUninstall`), if any.
#[tauri::command]
async fn confirm_remove(app: AppHandle, mode: String, id: String) -> R<bool> {
    let Some(warning) = read(&app, &mode)?.removal_warning(&id) else {
        return Ok(true);
    };
    Ok(app
        .dialog()
        .message(warning)
        .title("Warning")
        .kind(tauri_plugin_dialog::MessageDialogKind::Warning)
        .buttons(tauri_plugin_dialog::MessageDialogButtons::YesNo)
        .blocking_show())
}

// ---------- plugins ----------

#[tauri::command]
async fn plugins(app: AppHandle, mode: String) -> R<Vec<PluginRow>> {
    blocking(move || Ok(read(&app, &mode)?.plugins().unwrap_or_default())).await
}

/// Set plugin order + active states; refused (with the reasons) when it would break the load order.
#[tauri::command]
async fn plugins_set(app: AppHandle, mode: String, plugins: Vec<Plugin>) -> R<Vec<PluginRow>> {
    blocking(move || {
        let lock = app.state::<App>().lock(&mode);
        let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
        manager(&app, &mode)?.set_plugins(&plugins)
    })
    .await
}

/// Save the load order to a text file the user picks; false when they cancel.
#[tauri::command]
async fn plugins_export(app: AppHandle, mode: String) -> R<bool> {
    let text = blocking({
        let (app, mode) = (app.clone(), mode.clone());
        move || read(&app, &mode)?.export_load_order()
    })
    .await?;
    let Some(path) = app
        .dialog()
        .file()
        .add_filter("Load order", &["txt"])
        .set_file_name(format!("{mode} load order.txt"))
        .blocking_save_file()
    else {
        return Ok(false);
    };
    let path = path.into_path().map_err(|e| e.to_string())?;
    std::fs::write(&path, text).at(&path)?;
    Ok(true)
}

/// Apply a load order from a text file the user picks; None when they cancel.
#[tauri::command]
async fn plugins_import(app: AppHandle, mode: String) -> R<Option<ImportReport>> {
    let Some(path) = app
        .dialog()
        .file()
        .add_filter("Load order", &["txt"])
        .blocking_pick_file()
    else {
        return Ok(None);
    };
    let path = path.into_path().map_err(|e| e.to_string())?;
    blocking(move || {
        let text = std::fs::read(&path).at(&path)?;
        let lock = app.state::<App>().lock(&mode);
        let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
        manager(&app, &mode)?
            .import_load_order(&String::from_utf8_lossy(&text))
            .map(Some)
    })
    .await
}

/// Replace the game's LOOT userlist with one the user picks; false when they cancel.
#[tauri::command]
async fn loot_import_userlist(app: AppHandle, mode: String) -> R<bool> {
    let Some(path) = app
        .dialog()
        .file()
        .add_filter("LOOT userlist", &["yaml", "yml"])
        .blocking_pick_file()
    else {
        return Ok(false);
    };
    let path = path.into_path().map_err(|e| e.to_string())?;
    blocking(move || {
        read(&app, &mode)?
            .loot_import_userlist(&path)
            .map(|()| true)
    })
    .await
}

/// Task: LOOT's proposed order (refreshing a stale masterlist first); applied via `plugins_set`.
#[tauri::command]
fn loot_preview(app: AppHandle, id: String, mode: String) {
    spawn(&app, id, mode.clone(), "LOOT sort".into(), move |app, _| {
        manager(app, &mode)?.loot_preview()
    });
}

// ---------- Nexus metadata, categories, update checks ----------

/// `<mods dir>/categories.json`: the game's Nexus categories as last fetched.
#[derive(Serialize, Deserialize)]
struct CategoryCache {
    fetched: u64,
    list: Vec<nexus::Category>,
}

const CATEGORY_MAX_AGE: u64 = 7 * 86_400;
/// Automatic update checks run at most this often per game.
const AUTO_CHECK_EVERY: u64 = 6 * 3600;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn category_cache(m: &Manager) -> Option<CategoryCache> {
    serde_json::from_slice(&std::fs::read(m.mods_dir.join("categories.json")).ok()?).ok()
}

/// Fetched categories, else the ones bundled with the game mode.
fn categories(m: &Manager) -> Vec<nexus::Category> {
    category_cache(m)
        .map(|c| c.list)
        .unwrap_or_else(|| game::default_categories(&m.game.mode_id))
}

/// Re-fetch the category list when it's a week old.
fn refresh_categories(c: &nexus::Client, m: &Manager) -> R<()> {
    if category_cache(m).is_some_and(|c| now().saturating_sub(c.fetched) < CATEGORY_MAX_AGE) {
        return Ok(());
    }
    let list = c.categories(&m.game.nexus_domain())?;
    let path = m.mods_dir.join("categories.json");
    let cache = CategoryCache {
        fetched: now(),
        list,
    };
    nmm_core::write_durable(&path, &serde_json::to_vec_pretty(&cache).unwrap())
}

/// Identify unknown mods, refresh categories, look for updates; saves what it learnt.
fn run_check(app: &AppHandle, mode: &str, task: Task) -> R<nexus::updates::CheckReport> {
    let c = nexus_client(app)?;
    let mut m = manager(app, mode)?;
    let domain = m.game.nexus_domain();
    let mut report = nexus::updates::check(&c, &domain, &mut m.state.mods, now(), &task)?;
    if !report.errors.iter().any(|e| e.contains("rate limit")) {
        m.state.nexus_checked = Some(now());
    }
    m.save()?;
    if let Err(e) = refresh_categories(&c, &m) {
        report.errors.push(format!("categories: {e}"));
    }
    tracing::info!(
        game = mode,
        identified = report.identified,
        checked = report.checked,
        updates = report.updates.len(),
        errors = report.errors.len(),
        "Nexus update check"
    );
    Ok(report)
}

/// Task: match unknown mods by md5 and check for updates.
#[tauri::command]
fn nexus_check(app: AppHandle, id: String, mode: String) {
    spawn(
        &app,
        id,
        mode.clone(),
        "Nexus update check".into(),
        move |app, task| run_check(app, &mode, task),
    );
}

/// Check every configured game now, then every few hours, when logged in and the last check is stale.
fn auto_check(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(30));
        let logged_in = key::get(&app).is_some();
        if logged_in {
            for mode in load_config(&app).games.keys() {
                let stale = read(&app, mode).is_ok_and(|m| {
                    m.state
                        .nexus_checked
                        .is_none_or(|t| now().saturating_sub(t) >= AUTO_CHECK_EVERY)
                });
                if stale {
                    let mode2 = mode.clone();
                    spawn(
                        &app,
                        format!("auto-check-{mode}"),
                        mode.clone(),
                        "automatic Nexus update check".into(),
                        move |app, task| {
                            let r = run_check(app, &mode2, task);
                            let _ = app.emit("library-changed", &mode2);
                            r
                        },
                    );
                }
            }
        }
        std::thread::sleep(Duration::from_secs(AUTO_CHECK_EVERY / 6));
    });
}

/// Open a mod (or file) page on nexusmods.com in the browser.
#[tauri::command]
async fn open_nexus(app: AppHandle, domain: String, mod_id: u64, file_id: Option<u64>) -> R<()> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(nexus::link::mod_url(&domain, mod_id, file_id), None::<&str>)
        .map_err(|e| format!("open browser: {e}").into())
}

/// A mod's readme: plain text comes back to show in the app; other formats (HTML, RTF, PDF, Word)
/// open in the system's default app (C# `OpenReadMe` opened them all that way).
#[tauri::command]
async fn readme(app: AppHandle, mode: String, id: String, name: String) -> R<Option<String>> {
    let path = read(&app, &mode)?
        .readme_path(&id, &name)
        .ok_or("no such readme")?;
    if name.to_lowercase().ends_with(".txt") {
        let bytes = std::fs::read(&path).at(&path)?;
        return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
    }
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_path(path.to_string_lossy(), None::<&str>)
        .map_err(|e| format!("open readme: {e}"))?;
    Ok(None)
}

/// A mod's kept `fomod/screenshot*` image.
#[tauri::command]
async fn screenshot(app: AppHandle, mode: String, id: String) -> R<tauri::ipc::Response> {
    let path = read(&app, &mode)?
        .screenshot_path(&id)
        .ok_or("no screenshot")?;
    Ok(tauri::ipc::Response::new(std::fs::read(&path).at(&path)?))
}

#[tauri::command]
async fn pick_archive(app: AppHandle) -> Option<String> {
    app.dialog()
        .file()
        .add_filter(
            "Mod archives",
            &["zip", "7z", "rar", "fomod", "omod", "dazip"],
        )
        .blocking_pick_file()
        .map(|p| p.to_string())
}

#[tauri::command]
async fn pick_folder(app: AppHandle) -> Option<String> {
    app.dialog()
        .file()
        .blocking_pick_folder()
        .map(|p| p.to_string())
}

#[derive(Clone, Serialize)]
struct StageResult {
    token: Option<String>,
    fomod: Option<ModuleConfig>,
    /// Unsupported C# script installer: the UI offers a plain install (`fomod_install` without `sel`).
    script: Option<String>,
    /// OBMM Mod Script installer: run it with `script_step`, then `fomod_install` without `sel`.
    mod_script: bool,
    /// `fomod/info.xml` metadata, for the wizard header.
    info: Option<Info>,
    /// Choices this mod (or the one it replaces) was last installed with, to replay.
    choices: Option<Selection>,
    /// Installed mod this would replace: itself (reinstall) or an older version (upgrade).
    replaces: Option<ModEntry>,
    /// This archive's version (Nexus, else info.xml).
    version: Option<String>,
}

/// Task: extract an archive; plain mods install right away, FOMods return their config for the
/// wizard, C# script installers their script path. One that would replace an installed mod
/// (`replace`, the same archive, or an older version of it) waits for `fomod_install` too.
#[tauri::command]
fn stage(
    app: AppHandle,
    id: String,
    mode: String,
    archive: PathBuf,
    download: Option<String>,
    replace: Option<String>,
) {
    let what = format!("install {}", archive.display());
    spawn(&app, id, mode.clone(), what, move |app, task| {
        let mut m = manager(app, &mode)?;
        m.task = task;
        let mut staged = m.stage(&archive)?;
        staged.nexus = download
            .and_then(|id| downloads_of(app).ok()?.get(&id))
            .map(|d| download_meta(app, &d));
        let old = replace.or_else(|| staged.replaces.clone().or_else(|| m.older_version(&staged)));
        if let Some(old) = &old {
            m.set_replaces(&mut staged, old);
        }
        let replaces = old.and_then(|o| m.state.mods.iter().find(|x| x.id == o).cloned());
        let version = (staged.nexus.as_ref().map(|n| &n.version))
            .or(staged.info.as_ref().map(|i| &i.version))
            .filter(|v| !v.is_empty())
            .cloned();
        let (fomod, script) = (staged.fomod.clone().map(|f| f.1), staged.script.clone());
        let (info, choices) = (staged.info.clone(), staged.choices.clone());
        let mod_script = staged.mod_script.is_some();
        if fomod.is_none() && script.is_none() && !mod_script && replaces.is_none() {
            m.transact(|m| m.install(staged, None))?;
            return Ok(StageResult {
                token: None,
                fomod,
                script,
                mod_script,
                info,
                choices,
                replaces,
                version,
            });
        }
        let token = staged.id.clone();
        app.state::<App>()
            .staged
            .lock()
            .unwrap()
            .insert(token.clone(), (mode, staged));
        Ok(StageResult {
            token: Some(token),
            fomod,
            script,
            mod_script,
            info,
            choices,
            replaces,
            version,
        })
    });
}

fn staged(st: &State<'_, App>, token: &str) -> R<(String, Staged)> {
    st.staged
        .lock()
        .unwrap()
        .get(token)
        .cloned()
        .ok_or_else(|| "install session expired".into())
}

#[tauri::command]
async fn fomod_eval(app: AppHandle, st: State<'_, App>, token: String, sel: Selection) -> R<Eval> {
    let (mode, s) = staged(&st, &token)?;
    let m = read(&app, &mode)?;
    Ok(s.fomod.unwrap().1.evaluate(&sel, &m.fomod_env()))
}

#[tauri::command]
async fn fomod_image(st: State<'_, App>, token: String, path: String) -> R<tauri::ipc::Response> {
    let (_, s) = staged(&st, &token)?;
    let base = match (s.fomod, s.mod_script) {
        (Some((base, _)), _) => base,
        (_, Some(ms)) => ms.base,
        _ => return Err("no installer images".into()),
    };
    // Resolve case-insensitively and refuse anything outside the extracted archive.
    let want = path.replace('\\', "/").to_lowercase();
    let hit = walkdir::WalkDir::new(&base)
        .into_iter()
        .filter_map(Result::ok)
        .find(|e| {
            e.path()
                .strip_prefix(&base)
                .is_ok_and(|r| r.to_string_lossy().replace('\\', "/").to_lowercase() == want)
        })
        .ok_or("image not found")?;
    Ok(tauri::ipc::Response::new(
        std::fs::read(hit.path()).at(hit.path())?,
    ))
}

/// Run a staged Mod Script with the answers so far: its next question, or null once it ran to the
/// end (`fomod_install` without `sel` then installs what it chose).
#[tauri::command]
async fn script_step(app: AppHandle, token: String, answers: Vec<Answer>) -> R<Option<Prompt>> {
    blocking(move || {
        let st = app.state::<App>();
        let (mode, mut s) = staged(&st, &token)?;
        let next = read(&app, &mode)?.run_mod_script(&mut s, &answers)?;
        if let Some(e) = st.staged.lock().unwrap().get_mut(&token) {
            e.1 = s;
        }
        Ok(next)
    })
    .await
}

/// Task: install a staged FOMod with the wizard's selection; no selection = plain install of a
/// staged C# script mod, finished Mod Script or plain mod. `replace` false installs alongside the mod it would replace.
#[tauri::command]
fn fomod_install(
    app: AppHandle,
    st: State<'_, App>,
    id: String,
    token: String,
    sel: Option<Selection>,
    replace: bool,
) -> R<()> {
    let (mode, mut s) = staged(&st, &token)?;
    if !replace {
        s.replaces = None;
    }
    let what = format!("install FOMod {}", s.archive.display());
    spawn(&app, id, mode.clone(), what, move |app, task| {
        let mut m = manager(app, &mode)?;
        m.task = task;
        // Staging dir is consumed whether install succeeds, fails or is cancelled.
        app.state::<App>().staged.lock().unwrap().remove(&token);
        m.transact(|m| m.install(s, sel.as_ref()))
    });
    Ok(())
}

#[tauri::command]
async fn fomod_cancel(st: State<'_, App>, token: String) -> R<()> {
    if let Some((_, s)) = st.staged.lock().unwrap().remove(&token) {
        let _ = std::fs::remove_dir_all(s.dir);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Op {
    SetActive { id: String, active: bool },
    SetGameRoot { id: String, on: bool },
    SetLinkMode { mode: LinkMode },
    Remove { id: String },
    Reorder { id: String, delta: isize },
    SaveProfile { name: String },
    LoadProfile { name: String },
    DeleteProfile { name: String },
    Redeploy,
    SetArchiveInvalidation { on: bool },
    PurgeLoose,
}

/// Task: change intent and redeploy; a cancel rolls back to the previous intent.
#[tauri::command]
fn apply(app: AppHandle, id: String, mode: String, op: Op) {
    spawn(
        &app,
        id,
        mode.clone(),
        format!("{op:?}"),
        move |app, task| {
            let mut m = manager(app, &mode)?;
            m.task = task;
            m.transact(|m| run_op(m, op))
        },
    );
}

fn run_op(m: &mut Manager, op: Op) -> R<()> {
    match op {
        Op::SetActive { id, active } => m.set_active(&id, active),
        Op::SetGameRoot { id, on } => m.set_game_root(&id, on),
        Op::SetLinkMode { mode } => m.set_link_mode(mode),
        Op::Remove { id } => m.remove(&id),
        Op::Reorder { id, delta } => m.reorder(&id, delta),
        Op::SaveProfile { name } => m.save_profile(&name),
        Op::LoadProfile { name } => m.load_profile(&name),
        Op::DeleteProfile { name } => m.delete_profile(&name),
        Op::Redeploy => m.deploy(),
        Op::SetArchiveInvalidation { on } => ArchiveInvalidation::new(&m.game, &m.game_path)
            .ok_or("this game doesn't use archive invalidation")?
            .set(on),
        Op::PurgeLoose => m.purge_loose().map(|_| ()),
    }
}

/// Task: rebuild deploy records from the game folder, then redeploy (when the journal was lost).
#[tauri::command]
fn repair(app: AppHandle, id: String, mode: String) {
    spawn(&app, id, mode.clone(), "repair".into(), move |app, task| {
        let mut m = manager(app, &mode)?;
        m.task = task;
        let mut report = None;
        m.transact(|m| m.repair().map(|r| report = Some(r)))?;
        Ok(report.unwrap_or_default())
    });
}

/// Task: re-add stray mod folders; ones already live in the game come back enabled.
#[tauri::command]
fn adopt(app: AppHandle, id: String, mode: String) {
    spawn(
        &app,
        id,
        mode.clone(),
        "adopt stray mods".into(),
        move |app, task| {
            let mut m = manager(app, &mode)?;
            m.task = task;
            let mut report = None;
            m.transact(|m| m.adopt().map(|r| report = Some(r)))?;
            Ok(report.unwrap_or_default())
        },
    );
}

fn launch_settings(app: &AppHandle, mode: &str) -> launcher::Settings {
    let c = load_config(app).games.get(mode).cloned();
    launcher::Settings {
        custom: c.as_ref().and_then(|c| c.launch.clone()),
        custom_args: c.as_ref().and_then(|c| c.launch_args.clone()),
        tools: c.map(|c| c.tools).unwrap_or_default(),
    }
}

#[derive(Serialize)]
struct LaunchOptions {
    options: Vec<launcher::LaunchOption>,
    /// What plain Launch runs.
    default: String,
}

#[tauri::command]
async fn launch_options(app: AppHandle, mode: String) -> R<LaunchOptions> {
    blocking(move || {
        let m = read(&app, &mode)?;
        let env = discover::Env::system();
        let (options, default) = launcher::options(
            &m.game,
            &m.game_path,
            &launch_settings(&app, &mode),
            &env.registry,
        );
        Ok(LaunchOptions { options, default })
    })
    .await
}

/// Start launch option `id` (default: the game's default launch), through Proton / Wine for a
/// Windows program off Windows.
#[tauri::command]
async fn launch(app: AppHandle, mode: String, id: Option<String>) -> R<()> {
    // read() waits for this game's tasks, so the game never starts mid-deploy.
    let m = read(&app, &mode)?;
    let settings = launch_settings(&app, &mode);
    let env = discover::Env::system();
    let (options, default) = launcher::options(&m.game, &m.game_path, &settings, &env.registry);
    let id = id.unwrap_or(default);
    let opt = options
        .iter()
        .find(|o| o.id == id)
        .ok_or_else(|| format!("unknown launch option {id}"))?;
    let exe = opt.exe.clone().ok_or("not found; set its folder first")?;
    let args = match id.as_str() {
        launcher::CUSTOM => launcher::split_args(settings.custom_args.as_deref().unwrap_or("")),
        _ => launcher::tool(&m.game, &id)
            .map(|t| t.args(&m.game, &m.game_path))
            .unwrap_or_default(),
    };
    let plan = launcher::plan(
        &m.game,
        &m.game_path,
        &exe,
        args,
        id == launcher::PLAIN,
        &env.steam_roots,
    )?;
    std::process::Command::new(&plan.program)
        .args(&plan.args)
        .envs(plan.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&plan.cwd)
        .spawn()
        .at(&plan.program)?;
    tracing::info!(game = %mode, option = %id, program = %plan.program.display(), "launched");
    if load_config(&app).settings.close_on_launch && !id.starts_with(launcher::TOOL) {
        // Through ExitRequested, so tasks on other games still roll back first.
        app.exit(0);
    }
    Ok(())
}

/// Pick the folder of supported tool `id` (`tool:LOOT`); refused when its executable isn't there.
#[tauri::command]
async fn tool_folder(app: AppHandle, mode: String, id: String) -> R<bool> {
    let def = game::find(&mode).ok_or("unknown game")?;
    let tool = launcher::tool(&def, &id).ok_or("unknown tool")?;
    let Some(dir) = app.dialog().file().blocking_pick_folder() else {
        return Ok(false);
    };
    let dir = dir.into_path().map_err(|e| e.to_string())?;
    if tool.in_dir(&dir).is_none() {
        return Err(format!("{} doesn't contain {}", dir.display(), tool.name).into());
    }
    update_config(&app, |c| {
        if let Some(g) = c.games.get_mut(&mode) {
            g.tools.insert(tool.settings_key().into(), dir);
        }
    })?;
    Ok(true)
}

/// Yes/No question in a native dialog.
#[tauri::command]
async fn ask(app: AppHandle, title: String, message: String) -> bool {
    app.dialog()
        .message(message)
        .title(title)
        .kind(tauri_plugin_dialog::MessageDialogKind::Warning)
        .buttons(tauri_plugin_dialog::MessageDialogButtons::YesNo)
        .blocking_show()
}

/// Three-way question: Some(true) = `yes`, Some(false) = `no`, None = Cancel.
#[tauri::command]
async fn ask3(
    app: AppHandle,
    title: String,
    message: String,
    yes: String,
    no: String,
) -> Option<bool> {
    use tauri_plugin_dialog::{MessageDialogButtons, MessageDialogResult as A};
    let r = app
        .dialog()
        .message(message)
        .title(title)
        .buttons(MessageDialogButtons::YesNoCancelCustom(
            yes.clone(),
            no.clone(),
            "Cancel".into(),
        ))
        .blocking_show_with_result();
    match r {
        A::Yes => Some(true),
        A::No => Some(false),
        A::Custom(l) if l == yes => Some(true),
        A::Custom(l) if l == no => Some(false),
        _ => None,
    }
}

/// Loose files in the game's Data folder (not base game, not ours); None when the game has no base list.
#[tauri::command]
async fn loose_files(app: AppHandle, mode: String) -> R<Option<Vec<String>>> {
    blocking(move || Ok(read(&app, &mode)?.loose_files())).await
}

// ---------- downloads ----------

fn downloads_of(app: &AppHandle) -> R<Downloads> {
    Ok(app
        .state::<App>()
        .downloads
        .get()
        .ok_or("download queue unavailable; see the log")?
        .clone())
}

/// Open the queue in `<app data>/Downloads`; links and md5 checks go through the Nexus client.
fn open_downloads(app: &AppHandle) -> R<Downloads> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("Downloads");
    let (a, b, c) = (app.clone(), app.clone(), app.clone());
    let hooks = download::Hooks {
        links: Box::new(move |d| {
            let links =
                nexus_client(&a)?.download_links(&d.domain, d.mod_id, d.file_id, d.key.as_ref())?;
            Ok(links.into_iter().map(|l| l.url).collect())
        }),
        verify: Box::new(move |d, md5| {
            let hits = nexus_client(&b)?.by_md5(&d.domain, md5)?;
            Ok(hits
                .iter()
                .any(|h| h.mod_id == d.mod_id && h.file.file_id == d.file_id))
        }),
        changed: Box::new(move |d| {
            let _ = c.emit("download", d);
        }),
    };
    Downloads::open(&dir, load_config(app).settings.max_downloads, hooks)
}

#[tauri::command]
async fn downloads(app: AppHandle) -> R<Vec<Download>> {
    Ok(downloads_of(&app)?.list())
}

/// Queue a Nexus link (nxm:// or mod page; a page means its main file) for `mode`. Returns the id.
#[tauri::command]
async fn download_add(app: AppHandle, mode: String, url: String) -> R<String> {
    blocking(move || {
        let link = nexus::link::parse(&url).ok_or("not a Nexus mod link")?;
        let dl = downloads_of(&app)?;
        let c = nexus_client(&app)?;
        let file = match link.file_id {
            Some(f) => c.file(&link.domain, link.mod_id, f)?,
            None => c.default_file(&link.domain, link.mod_id)?,
        };
        let m = c.mod_info(&link.domain, link.mod_id)?;
        let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
        let name = if file.file_name.is_empty() {
            &file.name
        } else {
            &file.file_name
        };
        let id = dl.add(Download {
            id: String::new(),
            path: dir
                .join("Downloads")
                // One folder per storage group: SkyrimSE and SkyrimGOG share their downloads.
                .join(&game::storage_group(&mode)[0])
                .join(download::safe_file_name(name)),
            mode,
            domain: link.domain,
            mod_id: link.mod_id,
            file_id: file.file_id,
            mod_name: m.name,
            name: file.name,
            version: file.version,
            size: file.size,
            key: link.key,
            state: download::State::Queued,
            done: 0,
            error: None,
            verified: None,
        });
        Ok(id)
    })
    .await
}

#[tauri::command]
async fn download_pause(app: AppHandle, id: String) -> R<()> {
    downloads_of(&app)?.pause(&id);
    Ok(())
}

#[tauri::command]
async fn download_resume(app: AppHandle, id: String) -> R<()> {
    downloads_of(&app)?.resume(&id);
    Ok(())
}

#[tauri::command]
async fn download_remove(app: AppHandle, id: String, delete_file: bool) -> R<()> {
    downloads_of(&app)?.remove(&id, delete_file);
    Ok(())
}

/// What a finished download says about its mod; author/category from Nexus when reachable.
fn download_meta(app: &AppHandle, d: &Download) -> nexus::updates::NexusMeta {
    let info = nexus_client(app).and_then(|c| c.mod_info(&d.domain, d.mod_id));
    if let Err(e) = &info {
        tracing::warn!(download = %d.id, "mod info for install: {e}");
    }
    let info = info.ok();
    nexus::updates::NexusMeta {
        domain: d.domain.clone(),
        mod_id: d.mod_id,
        file_id: d.file_id,
        mod_name: d.mod_name.clone(),
        file_name: d.name.clone(),
        version: d.version.clone(),
        author: info.as_ref().map(|i| i.author.clone()).unwrap_or_default(),
        category_id: info.and_then(|i| i.category_id),
        update: None,
        checked: None,
    }
}

/// Daily log files in the app log dir (7 kept), plus stderr; panics are logged too.
fn init_logging(app: &AppHandle) {
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let Ok(dir) = app.path().app_log_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir); // else the appender's old-file pruning errors on first run
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("nmm")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir);
    let file = match file {
        Ok(f) => f,
        Err(e) => return eprintln!("logging disabled: {}: {e}", dir.display()),
    };
    tracing_subscriber::fmt()
        .with_writer(
            file.and(std::io::stderr)
                .with_max_level(tracing::Level::INFO),
        )
        .with_ansi(false)
        .init();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default(info);
    }));
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "started");
}

fn main() {
    // Closing/quitting mid-task cancels it and waits for the rollback, so the game is never left
    // half-modded (a hard kill is covered by Manager::open's recovery instead).
    tauri::Builder::default()
        // First: a second launch (e.g. the browser opening an nxm:// link) hands its args here and exits.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            argv.iter().skip(1).for_each(|a| receive_link(app, a));
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .on_window_event(|w, e| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = e {
                if w.state::<App>().hold_quit(w.app_handle()) {
                    api.prevent_close();
                }
            }
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            init_logging(app.handle());
            use tauri_plugin_deep_link::DeepLinkExt;
            // Bundles register nxm:// at install; Windows/Linux dev builds register at runtime.
            #[cfg(any(windows, target_os = "linux"))]
            if let Err(e) = app.deep_link().register_all() {
                tracing::error!("registering nxm:// handler: {e}");
            }
            let h = app.handle().clone();
            // macOS delivers links as OS events, at startup too.
            app.deep_link()
                .on_open_url(move |e| e.urls().iter().for_each(|u| receive_link(&h, u.as_str())));
            match open_downloads(app.handle()) {
                Ok(d) => {
                    let _ = app.state::<App>().downloads.set(d);
                }
                Err(e) => tracing::error!("download queue: {e}"),
            }
            auto_check(app.handle().clone());
            // Windows/Linux pass the link as the only argument.
            std::env::args()
                .skip(1)
                .for_each(|a| receive_link(app.handle(), &a));
            Ok(())
        })
        .manage(App::default())
        .invoke_handler(tauri::generate_handler![
            games,
            configure,
            library,
            confirm_remove,
            launch_options,
            tool_folder,
            ask,
            ask3,
            loose_files,
            plugins,
            plugins_set,
            plugins_export,
            plugins_import,
            loot_import_userlist,
            loot_preview,
            pick_archive,
            pick_folder,
            stage,
            fomod_eval,
            fomod_image,
            fomod_install,
            fomod_cancel,
            script_step,
            apply,
            repair,
            adopt,
            launch,
            cancel,
            settings,
            save_settings,
            nexus_account,
            nexus_set_key,
            nexus_login,
            nexus_logout,
            take_links,
            downloads,
            download_add,
            download_pause,
            download_resume,
            download_remove,
            nexus_check,
            open_nexus,
            readme,
            screenshot
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, e| {
            if let tauri::RunEvent::ExitRequested { api, .. } = e {
                if app.state::<App>().hold_quit(app) {
                    api.prevent_exit();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config.json from before settings existed keeps its games and gets default settings.
    #[test]
    fn old_config_loads() {
        let cfg: Config =
            serde_json::from_str(r#"{"games":{"Fallout4":{"game_path":"/g","mods_dir":"/m"}}}"#)
                .unwrap();
        assert_eq!(cfg.games["Fallout4"].mods_dir, PathBuf::from("/m"));
        assert!(cfg.settings.mods_root.is_none() && !cfg.settings.close_on_launch);
        assert_eq!(cfg.settings.link_mode, LinkMode::Auto);
        assert_eq!(cfg.settings.max_downloads, 3);
        let back: Config = serde_json::from_slice(&serde_json::to_vec(&cfg).unwrap()).unwrap();
        assert_eq!(back.settings.nexus_api, nmm_core::nexus::Api::Graphql);
    }
}
