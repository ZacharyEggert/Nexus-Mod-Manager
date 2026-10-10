#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use nmm_core::fomod::{Eval, ModuleConfig, Selection};
use nmm_core::manager::{LinkMode, Manager, ModEntry, Staged};
use nmm_core::{discover, game, IoContext, Task};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    /// Where newly set-up games keep their mods (`<root>/<game>`); `None` = app data dir.
    mods_root: Option<PathBuf>,
    /// Link mode newly set-up games start with.
    link_mode: LinkMode,
    nexus_api: nmm_core::nexus::Api,
    /// Quit after launching a game.
    close_on_launch: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct GameConfig {
    game_path: PathBuf,
    mods_dir: PathBuf,
    /// Custom launch command (e.g. SMAPI); `None` = the definition's executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch: Option<String>,
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
fn manager(app: &AppHandle, mode: &str) -> R<Manager> {
    let def = game::find(mode).ok_or("unknown game")?;
    let cfg = load_config(app)
        .games
        .get(mode)
        .cloned()
        .ok_or("game path not set")?;
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
}

#[tauri::command]
async fn games(app: AppHandle) -> Vec<GameInfo> {
    let cfg = load_config(&app);
    let mut v: Vec<GameInfo> = game::all()
        .into_iter()
        .map(|g| {
            let c = cfg.games.get(&g.mode_id);
            GameInfo {
                detected: discover::detect(&g),
                game_path: c.map(|c| c.game_path.clone()),
                mods_dir: c.map(|c| c.mods_dir.clone()),
                launch: c.and_then(|c| c.launch.clone()),
                default_launch: g.default_executable().map(String::from),
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
        c.games.insert(
            mode.clone(),
            GameConfig {
                game_path,
                mods_dir,
                launch: launch.filter(|l| !l.trim().is_empty()),
            },
        );
    })?;
    if fresh {
        // A new mods folder starts with the default link mode; nothing is deployed yet, so no relink.
        let mut m = read(&app, &mode)?;
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
    update_config(&app, |c| c.settings = settings)
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
        mods: m.state.mods,
    })
}

#[tauri::command]
async fn pick_archive(app: AppHandle) -> Option<String> {
    app.dialog()
        .file()
        .add_filter("Mod archives", &["zip", "7z", "rar", "fomod"])
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
}

/// Task: extract an archive; plain mods install right away, FOMods return their config for the wizard.
#[tauri::command]
fn stage(app: AppHandle, id: String, mode: String, archive: PathBuf) {
    let what = format!("install {}", archive.display());
    spawn(&app, id, mode.clone(), what, move |app, task| {
        let mut m = manager(app, &mode)?;
        m.task = task;
        let staged = m.stage(&archive)?;
        match staged.fomod.clone() {
            None => {
                m.transact(|m| m.install(staged, None))?;
                Ok(StageResult {
                    token: None,
                    fomod: None,
                })
            }
            Some((_, cfg)) => {
                let token = staged.id.clone();
                app.state::<App>()
                    .staged
                    .lock()
                    .unwrap()
                    .insert(token.clone(), (mode, staged));
                Ok(StageResult {
                    token: Some(token),
                    fomod: Some(cfg),
                })
            }
        }
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
    Ok(s.fomod.unwrap().1.evaluate(&sel, &|n| m.file_state(n)))
}

#[tauri::command]
async fn fomod_image(st: State<'_, App>, token: String, path: String) -> R<tauri::ipc::Response> {
    let (_, s) = staged(&st, &token)?;
    let base = s.fomod.unwrap().0;
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

/// Task: install a staged FOMod with the wizard's selection.
#[tauri::command]
fn fomod_install(
    app: AppHandle,
    st: State<'_, App>,
    id: String,
    token: String,
    sel: Selection,
) -> R<()> {
    let (mode, s) = staged(&st, &token)?;
    let what = format!("install FOMod {}", s.archive.display());
    spawn(&app, id, mode.clone(), what, move |app, task| {
        let mut m = manager(app, &mode)?;
        m.task = task;
        // Staging dir is consumed whether install succeeds, fails or is cancelled.
        app.state::<App>().staged.lock().unwrap().remove(&token);
        m.transact(|m| m.install(s, Some(&sel)))
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

#[tauri::command]
async fn launch(app: AppHandle, mode: String) -> R<()> {
    // read() waits for this game's tasks, so the game never starts mid-deploy.
    let m = read(&app, &mode)?;
    let custom = load_config(&app)
        .games
        .get(&mode)
        .and_then(|c| c.launch.clone());
    let exe = m
        .game
        .executable(&m.game_path, custom.as_deref())
        .ok_or("no executable defined")?;
    std::process::Command::new(&exe)
        .current_dir(&m.game_path)
        .spawn()
        .at(&exe)?;
    tracing::info!(game = %mode, exe = %exe.display(), "launched");
    if load_config(&app).settings.close_on_launch {
        // Through ExitRequested, so tasks on other games still roll back first.
        app.exit(0);
    }
    Ok(())
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
        .on_window_event(|w, e| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = e {
                if w.state::<App>().hold_quit(w.app_handle()) {
                    api.prevent_close();
                }
            }
        })
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            init_logging(app.handle());
            Ok(())
        })
        .manage(App::default())
        .invoke_handler(tauri::generate_handler![
            games,
            configure,
            library,
            pick_archive,
            pick_folder,
            stage,
            fomod_eval,
            fomod_image,
            fomod_install,
            fomod_cancel,
            apply,
            repair,
            adopt,
            launch,
            cancel,
            settings,
            save_settings
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
        let back: Config = serde_json::from_slice(&serde_json::to_vec(&cfg).unwrap()).unwrap();
        assert_eq!(back.settings.nexus_api, nmm_core::nexus::Api::Graphql);
    }
}
