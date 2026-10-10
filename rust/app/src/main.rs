#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use nmm_core::fomod::{Eval, ModuleConfig, Selection};
use nmm_core::manager::{Manager, ModEntry, Staged};
use nmm_core::{discover, game};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Manager as _, State};
use tauri_plugin_dialog::DialogExt;

type R<T> = Result<T, String>;

#[derive(Default, Serialize, Deserialize)]
struct Config {
    games: BTreeMap<String, GameConfig>,
}

#[derive(Clone, Serialize, Deserialize)]
struct GameConfig {
    game_path: PathBuf,
    mods_dir: PathBuf,
}

/// ponytail: one global lock serialises every mutation; per-game locks if that ever matters.
#[derive(Default)]
struct App {
    lock: Mutex<()>,
    staged: Mutex<HashMap<String, (String, Staged)>>,
}

fn config_path(app: &AppHandle) -> R<PathBuf> {
    Ok(app.path().app_config_dir().map_err(|e| e.to_string())?.join("config.json"))
}

fn load_config(app: &AppHandle) -> Config {
    config_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn default_mods_dir(app: &AppHandle, mode: &str) -> R<PathBuf> {
    Ok(app.path().app_data_dir().map_err(|e| e.to_string())?.join("Mods").join(mode))
}

fn manager(app: &AppHandle, mode: &str) -> R<Manager> {
    let def = game::find(mode).ok_or("unknown game")?;
    let cfg = load_config(app).games.get(mode).cloned().ok_or("game path not set")?;
    Manager::open(def, cfg.game_path, cfg.mods_dir)
}

#[derive(Serialize)]
struct GameInfo {
    mode_id: String,
    name: String,
    game_path: Option<PathBuf>,
    mods_dir: Option<PathBuf>,
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
                mode_id: g.mode_id,
                name: g.name,
            }
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

#[tauri::command]
async fn configure(app: AppHandle, mode: String, game_path: PathBuf, mods_dir: Option<PathBuf>) -> R<()> {
    if !game_path.is_dir() {
        return Err(format!("{} is not a folder", game_path.display()));
    }
    let mods_dir = match mods_dir {
        Some(d) => d,
        None => default_mods_dir(&app, &mode)?,
    };
    let mut cfg = load_config(&app);
    cfg.games.insert(mode, GameConfig { game_path, mods_dir });
    let path = config_path(&app)?;
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(path, serde_json::to_vec_pretty(&cfg).unwrap()).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct Library {
    mods: Vec<ModEntry>,
    conflicts: BTreeMap<String, Vec<String>>,
    profiles: Vec<String>,
    install_root: PathBuf,
}

#[tauri::command]
async fn library(app: AppHandle, mode: String) -> R<Library> {
    let m = manager(&app, &mode)?;
    Ok(Library {
        conflicts: m.conflicts(),
        install_root: m.install_root(),
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
    app.dialog().file().blocking_pick_folder().map(|p| p.to_string())
}

#[derive(Serialize)]
struct StageResult {
    token: Option<String>,
    fomod: Option<ModuleConfig>,
}

/// Extract an archive; plain mods install right away, FOMods return their config for the wizard.
#[tauri::command]
async fn stage(app: AppHandle, st: State<'_, App>, mode: String, archive: PathBuf) -> R<StageResult> {
    let _g = st.lock.lock().unwrap();
    let mut m = manager(&app, &mode)?;
    let staged = m.stage(&archive)?;
    match staged.fomod.clone() {
        None => {
            m.install(staged, None)?;
            Ok(StageResult { token: None, fomod: None })
        }
        Some((_, cfg)) => {
            let token = staged.id.clone();
            st.staged.lock().unwrap().insert(token.clone(), (mode, staged));
            Ok(StageResult { token: Some(token), fomod: Some(cfg) })
        }
    }
}

fn staged(st: &State<'_, App>, token: &str) -> R<(String, Staged)> {
    st.staged.lock().unwrap().get(token).cloned().ok_or_else(|| "install session expired".into())
}

#[tauri::command]
async fn fomod_eval(app: AppHandle, st: State<'_, App>, token: String, sel: Selection) -> R<Eval> {
    let (mode, s) = staged(&st, &token)?;
    let m = manager(&app, &mode)?;
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
        .find(|e| e.path().strip_prefix(&base).is_ok_and(|r| r.to_string_lossy().replace('\\', "/").to_lowercase() == want))
        .ok_or("image not found")?;
    Ok(tauri::ipc::Response::new(std::fs::read(hit.path()).map_err(|e| e.to_string())?))
}

#[tauri::command]
async fn fomod_install(app: AppHandle, st: State<'_, App>, token: String, sel: Selection) -> R<()> {
    let _g = st.lock.lock().unwrap();
    let (mode, s) = staged(&st, &token)?;
    manager(&app, &mode)?.install(s, Some(&sel))?;
    st.staged.lock().unwrap().remove(&token);
    Ok(())
}

#[tauri::command]
async fn fomod_cancel(st: State<'_, App>, token: String) -> R<()> {
    if let Some((_, s)) = st.staged.lock().unwrap().remove(&token) {
        let _ = std::fs::remove_dir_all(s.dir);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Op {
    SetActive { id: String, active: bool },
    Remove { id: String },
    Reorder { id: String, delta: isize },
    SaveProfile { name: String },
    LoadProfile { name: String },
    DeleteProfile { name: String },
    Redeploy,
}

#[tauri::command]
async fn apply(app: AppHandle, st: State<'_, App>, mode: String, op: Op) -> R<()> {
    let _g = st.lock.lock().unwrap();
    let mut m = manager(&app, &mode)?;
    match op {
        Op::SetActive { id, active } => m.set_active(&id, active),
        Op::Remove { id } => m.remove(&id),
        Op::Reorder { id, delta } => m.reorder(&id, delta),
        Op::SaveProfile { name } => m.save_profile(&name),
        Op::LoadProfile { name } => m.load_profile(&name),
        Op::DeleteProfile { name } => m.delete_profile(&name),
        Op::Redeploy => m.deploy(),
    }
}

#[tauri::command]
async fn launch(app: AppHandle, mode: String) -> R<()> {
    let m = manager(&app, &mode)?;
    let exe = m.game.executable(&m.game_path).ok_or("no executable defined")?;
    std::process::Command::new(&exe).current_dir(&m.game_path).spawn().map_err(|e| format!("{}: {e}", exe.display()))?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(App::default())
        .invoke_handler(tauri::generate_handler![
            games, configure, library, pick_archive, pick_folder, stage, fomod_eval, fomod_image, fomod_install,
            fomod_cancel, apply, launch
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
