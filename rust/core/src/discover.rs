//! Locate installed games from `discovery.stores`, in definition order (C#
//! `GameStoreInstallationPathDetector`): Steam libraries, GOG Galaxy and plain registry keys,
//! Epic `.item` manifests, Xbox / Microsoft Store folders. A hit must hold the store's
//! executable (C# `ValidatePath`).

use crate::game::{GameDef, Store};
use std::path::{Component, Path, PathBuf};

/// `(full key, value name)` -> string value, e.g. `HKEY_LOCAL_MACHINE\SOFTWARE\...`, `PATH`.
pub type Registry = Box<dyn Fn(&str, &str) -> Option<String>>;

/// Where detection looks. `Env::system()` is this machine; tests build their own.
pub struct Env {
    /// Steam install folders (each has `steamapps/`).
    pub steam_roots: Vec<PathBuf>,
    pub registry: Registry,
    /// `%ProgramData%\Epic\EpicGamesLauncher\Data\Manifests`.
    pub epic_manifests: Option<PathBuf>,
    /// `XboxGames` / `ModifiableWindowsApps` folders.
    pub xbox_roots: Vec<PathBuf>,
}

impl Env {
    pub fn system() -> Self {
        let registry: Registry = Box::new(registry_value);
        let mut steam_roots = vec![];
        // Steam stores its own folder (with `/`s) under HKCU; the defaults cover a missing key.
        if let Some(p) = registry_value(r"HKEY_CURRENT_USER\Software\Valve\Steam", "SteamPath") {
            steam_roots.push(PathBuf::from(p.replace('/', "\\")));
        }
        steam_roots.extend([
            PathBuf::from(r"C:\Program Files (x86)\Steam"),
            PathBuf::from(r"C:\Program Files\Steam"),
        ]);
        if let Some(h) = dirs::home_dir() {
            steam_roots.push(h.join(".steam/steam"));
            steam_roots.push(h.join(".local/share/Steam"));
            steam_roots.push(h.join("Library/Application Support/Steam"));
        }
        steam_roots.retain(|p| p.is_dir());
        steam_roots.dedup();
        let program_data = std::env::var_os("ProgramData").map(PathBuf::from);
        let mut xbox_roots = vec![];
        if cfg!(windows) {
            if let Some(pf) = std::env::var_os("ProgramFiles") {
                xbox_roots.push(PathBuf::from(pf).join("ModifiableWindowsApps"));
            }
            for d in b'A'..=b'Z' {
                let drive = PathBuf::from(format!("{}:\\", d as char));
                xbox_roots.push(drive.join("XboxGames"));
                xbox_roots.push(drive.join("ModifiableWindowsApps"));
            }
            xbox_roots.retain(|p| p.is_dir());
        }
        Self {
            steam_roots,
            registry,
            epic_manifests: program_data.map(|p| p.join(r"Epic\EpicGamesLauncher\Data\Manifests")),
            xbox_roots,
        }
    }
}

#[cfg(windows)]
fn registry_value(key: &str, value: &str) -> Option<String> {
    use winreg::enums::{HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, HKEY_USERS};
    let (hive, path) = key.split_once('\\')?;
    let hive = match hive.to_ascii_uppercase().as_str() {
        "HKEY_LOCAL_MACHINE" | "HKLM" => HKEY_LOCAL_MACHINE,
        "HKEY_CURRENT_USER" | "HKCU" => HKEY_CURRENT_USER,
        "HKEY_CLASSES_ROOT" | "HKCR" => HKEY_CLASSES_ROOT,
        "HKEY_USERS" | "HKU" => HKEY_USERS,
        _ => return None,
    };
    let k = winreg::RegKey::predef(hive).open_subkey(path).ok()?;
    match k.get_value::<String, _>(value) {
        Ok(s) => Some(s),
        Err(_) => k.get_value::<u32, _>(value).ok().map(|n| n.to_string()),
    }
}

// ponytail: no registry off Windows; Wine prefixes' system.reg could be parsed if a store needs it.
#[cfg(not(windows))]
fn registry_value(_: &str, _: &str) -> Option<String> {
    None
}

/// Library roots listed in `libraryfolders.vdf` (`"path"  "D:\\SteamLibrary"` lines).
fn libraries(root: &Path) -> Vec<PathBuf> {
    let mut libs = vec![root.to_path_buf()];
    for vdf in [
        "steamapps/libraryfolders.vdf",
        "SteamApps/libraryfolders.vdf",
    ] {
        if let Ok(vdf) = std::fs::read_to_string(root.join(vdf)) {
            for line in vdf.lines() {
                let parts: Vec<&str> = line.split('"').collect();
                if parts.len() >= 4 && parts[1] == "path" {
                    let p = PathBuf::from(parts[3].replace("\\\\", "\\"));
                    if !libs.contains(&p) {
                        libs.push(p);
                    }
                }
            }
            break;
        }
    }
    libs
}

/// A definition's relative Windows path (`Zenimax Online\\The Elder Scrolls Online`) as a native
/// one; `..` steps up (TESO's launcher key points beside the game).
fn native(rel: &str) -> PathBuf {
    rel.split(['\\', '/']).filter(|c| !c.is_empty()).collect()
}

/// `a/b/../c` -> `a/c`, without touching the disk.
fn tidy(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir if out.file_name().is_some() => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// `%VAR%` -> its value (C# `ExpandEnvironmentVariables`); unknown ones stay as written.
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) if j > 0 => match std::env::var(&after[..j]) {
                Ok(v) => {
                    out.push_str(&v);
                    rest = &after[j + 1..];
                }
                Err(_) => {
                    out.push('%');
                    rest = after;
                }
            },
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// C# `ValidatePath`: env vars, `pathSuffix`, folder exists, the executable is in it. A native
/// mac/Linux build counts when it ships the executable without `.exe`.
fn validate(path: &str, s: &Store) -> Option<PathBuf> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let mut p = PathBuf::from(expand_env(path));
    if let Some(x) = &s.path_suffix {
        p = tidy(&p.join(native(x)));
    }
    if !p.is_dir() {
        return None;
    }
    if let Some(exe) = &s.executable_name {
        let native_exe = exe
            .strip_suffix(".exe")
            .or_else(|| exe.strip_suffix(".EXE"))
            .unwrap_or(exe);
        let found = crate::manager::exists_ci(&p, exe)
            || (native_exe != exe && crate::manager::exists_ci(&p, native_exe));
        if !found {
            return None;
        }
    }
    Some(p)
}

fn steam(s: &Store, env: &Env) -> Option<PathBuf> {
    let folder = native(s.install_folder_name.as_deref()?);
    let in_library = env
        .steam_roots
        .iter()
        .flat_map(|r| libraries(r))
        .find_map(|lib| {
            validate(
                &lib.join("steamapps/common").join(&folder).to_string_lossy(),
                s,
            )
        });
    in_library.or_else(|| {
        // Steam's own uninstall entry, for libraries libraryfolders.vdf doesn't list.
        let id = s.id.as_deref()?;
        let key = format!(
            r"HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Steam App {id}"
        );
        validate(&(env.registry)(&key, "InstallLocation")?, s)
    })
}

fn gog(s: &Store, env: &Env) -> Option<PathBuf> {
    let value = s.registry_value_name.as_deref().unwrap_or("PATH");
    if let Some(k) = &s.registry_key {
        return validate(&(env.registry)(k, value)?, s);
    }
    let id = s.id.as_deref()?;
    [
        format!(r"HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\GOG.com\Games\{id}"),
        format!(r"HKEY_LOCAL_MACHINE\SOFTWARE\GOG.com\Games\{id}"),
        format!(r"HKEY_CURRENT_USER\SOFTWARE\GOG.com\Games\{id}"),
    ]
    .iter()
    .find_map(|k| validate(&(env.registry)(k, value)?, s))
}

fn registry(s: &Store, env: &Env) -> Option<PathBuf> {
    let value = s
        .registry_value_name
        .as_deref()
        .unwrap_or("InstallLocation");
    validate(&(env.registry)(s.registry_key.as_deref()?, value)?, s)
}

fn epic(s: &Store, env: &Env) -> Option<PathBuf> {
    let dir = env.epic_manifests.as_ref()?;
    let mut items: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("item"))
        })
        .collect();
    items.sort();
    items.iter().find_map(|item| {
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(item).ok()?).ok()?;
        let field = |k: &str| v.get(k).and_then(|x| x.as_str());
        let by_id = s.id.as_deref().is_some_and(|id| {
            ["AppName", "CatalogItemId", "MainGameCatalogItemId"]
                .iter()
                .any(|k| field(k).is_some_and(|f| f.eq_ignore_ascii_case(id)))
        });
        let by_name = s
            .install_folder_name
            .as_deref()
            .is_some_and(|n| field("DisplayName").is_some_and(|d| d.eq_ignore_ascii_case(n)));
        (by_id || by_name)
            .then(|| validate(field("InstallLocation")?, s))
            .flatten()
    })
}

fn xbox(s: &Store, env: &Env) -> Option<PathBuf> {
    if let Some(k) = &s.registry_key {
        let value = s
            .registry_value_name
            .as_deref()
            .unwrap_or("InstallLocation");
        if let Some(p) = (env.registry)(k, value).and_then(|p| validate(&p, s)) {
            return Some(p);
        }
    }
    let matches = |p: &Path| {
        if !p.is_dir() {
            return false;
        }
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if s.install_folder_name
            .as_deref()
            .is_some_and(|f| name.eq_ignore_ascii_case(f))
        {
            return true;
        }
        let Some(id) = s.id.as_deref() else {
            return true;
        };
        name.eq_ignore_ascii_case(id)
            || std::fs::read_to_string(p.join("MicrosoftGame.config"))
                .is_ok_and(|c| c.to_lowercase().contains(&id.to_lowercase()))
    };
    env.xbox_roots.iter().find_map(|root| {
        let mut cands: Vec<PathBuf> = vec![];
        for named in [s.install_folder_name.as_deref(), s.id.as_deref()]
            .into_iter()
            .flatten()
        {
            let p = root.join(native(named));
            cands.push(p.clone());
            cands.push(p.join("Content"));
        }
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for d in dirs {
            if matches(&d) {
                cands.push(d.clone());
            }
            if matches(&d.join("Content")) {
                cands.push(d.join("Content"));
            }
        }
        cands.iter().find_map(|c| validate(&c.to_string_lossy(), s))
    })
}

pub fn detect(game: &GameDef) -> Option<PathBuf> {
    detect_in(game, &Env::system())
}

pub fn detect_in(game: &GameDef, env: &Env) -> Option<PathBuf> {
    game.discovery
        .stores
        .iter()
        .find_map(|s| match s.store.to_ascii_lowercase().as_str() {
            "steam" => steam(s, env),
            "gog" => gog(s, env),
            "epic" => epic(s, env),
            "xbox" => xbox(s, env),
            "registry" => registry(s, env),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    fn env(reg: &[(&str, &str, &str)]) -> Env {
        let map: HashMap<(String, String), String> = reg
            .iter()
            .map(|(k, v, d)| ((k.to_lowercase(), v.to_lowercase()), d.to_string()))
            .collect();
        Env {
            steam_roots: vec![],
            registry: Box::new(move |k, v| map.get(&(k.to_lowercase(), v.to_lowercase())).cloned()),
            epic_manifests: None,
            xbox_roots: vec![],
        }
    }

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "").unwrap();
    }

    fn game(stores: serde_json::Value) -> GameDef {
        serde_json::from_value(serde_json::json!({
            "modeId": "X", "name": "X", "discovery": { "stores": stores }
        }))
        .unwrap()
    }

    #[test]
    fn steam_libraries_suffix_and_exe_check() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("Steam");
        let lib = t.path().join("Lib");
        fs::create_dir_all(root.join("steamapps")).unwrap();
        fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            format!(
                "\"libraryfolders\"\n{{\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
                lib.display()
            ),
        )
        .unwrap();
        let mut e = env(&[]);
        e.steam_roots = vec![root.clone()];
        let ds = crate::game::find("DarkSouls").unwrap();
        // Folder without the executable: not a hit.
        fs::create_dir_all(lib.join("steamapps/common/Dark Souls Prepare to Die Edition/DATA"))
            .unwrap();
        assert_eq!(detect_in(&ds, &e), None);
        let data = lib.join("steamapps/common/Dark Souls Prepare to Die Edition/DATA");
        touch(&data.join("DARKSOULS.exe"));
        assert_eq!(detect_in(&ds, &e), Some(data));
        // Backslash folder names (TESO) and native builds without .exe.
        let teso = crate::game::find("TESO").unwrap();
        let eso = root.join("steamapps/common/Zenimax Online/The Elder Scrolls Online");
        touch(&eso.join("game/client/eso"));
        assert_eq!(detect_in(&teso, &e), Some(eso));
        // Not in a library: Steam's uninstall key.
        let w3 = crate::game::find("Witcher3").unwrap();
        let elsewhere = t.path().join("W3");
        touch(&elsewhere.join("bin/x64/witcher3.exe"));
        let key = r"HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Steam App 292030";
        let mut e2 = env(&[(key, "InstallLocation", &elsewhere.to_string_lossy())]);
        e2.steam_roots = vec![root];
        assert_eq!(detect_in(&w3, &e2), Some(elsewhere));
    }

    #[test]
    fn gog_and_registry_stores() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("GOG Games/Stardew Valley");
        touch(&dir.join("Stardew Valley.exe"));
        let sdv = crate::game::find("stardewvalley").unwrap();
        let key = r"HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\GOG.com\Games\1453375253";
        assert_eq!(
            detect_in(&sdv, &env(&[(key, "PATH", &dir.to_string_lossy())])),
            Some(dir.clone())
        );
        // GOG by id only: tries the 64-bit, 32-bit and per-user keys.
        let g = game(
            serde_json::json!([{ "store": "GOG", "id": "42", "executableName": "Stardew Valley.exe" }]),
        );
        let k = r"HKEY_CURRENT_USER\SOFTWARE\GOG.com\Games\42";
        assert_eq!(
            detect_in(&g, &env(&[(k, "PATH", &dir.to_string_lossy())])),
            Some(dir.clone())
        );
        // Registry store, default value InstallLocation, %VAR% expansion, `..` suffix.
        std::env::set_var("NMM_TEST_ROOT", t.path());
        let g = game(serde_json::json!([{
            "store": "Registry", "registryKey": r"HKEY_LOCAL_MACHINE\Software\X",
            "pathSuffix": r"..\Stardew Valley", "executableName": "Stardew Valley.exe"
        }]));
        let e = env(&[(
            r"HKEY_LOCAL_MACHINE\Software\X",
            "InstallLocation",
            r"%NMM_TEST_ROOT%/GOG Games/Launcher",
        )]);
        assert_eq!(detect_in(&g, &e), Some(dir));
    }

    #[test]
    fn epic_manifests_and_xbox_folders() {
        let t = tempfile::tempdir().unwrap();
        let game_dir = t.path().join("Epic/Game");
        touch(&game_dir.join("Game.exe"));
        let manifests = t.path().join("Manifests");
        fs::create_dir_all(&manifests).unwrap();
        fs::write(
            manifests.join("a.item"),
            r#"{"AppName":"Other","InstallLocation":"/nope"}"#,
        )
        .unwrap();
        fs::write(
            manifests.join("b.item"),
            serde_json::json!({"AppName": "Fennec", "DisplayName": "Game", "InstallLocation": game_dir}).to_string(),
        )
        .unwrap();
        let mut e = env(&[]);
        e.epic_manifests = Some(manifests);
        let by_id = game(
            serde_json::json!([{ "store": "Epic", "id": "fennec", "executableName": "Game.exe" }]),
        );
        assert_eq!(detect_in(&by_id, &e), Some(game_dir.clone()));
        let by_name = game(serde_json::json!([{ "store": "Epic", "installFolderName": "Game" }]));
        assert_eq!(detect_in(&by_name, &e), Some(game_dir));

        let xbox = t.path().join("XboxGames");
        let content = xbox.join("Some Folder/Content");
        touch(&content.join("Game.exe"));
        fs::write(
            content.join("MicrosoftGame.config"),
            "<Game><Identity Name=\"Bethesda.Cool\"/></Game>",
        )
        .unwrap();
        e.xbox_roots = vec![xbox.clone()];
        let by_cfg = game(
            serde_json::json!([{ "store": "Xbox", "id": "bethesda.cool", "executableName": "Game.exe" }]),
        );
        assert_eq!(detect_in(&by_cfg, &e), Some(content.clone()));
        let named = xbox.join("Named Game/Content");
        touch(&named.join("Game.exe"));
        let by_folder = game(
            serde_json::json!([{ "store": "Xbox", "installFolderName": "Named Game", "executableName": "Game.exe" }]),
        );
        assert_eq!(detect_in(&by_folder, &e), Some(named));
        // First store in definition order wins.
        let both = game(serde_json::json!([
            { "store": "Xbox", "id": "bethesda.cool", "executableName": "Game.exe" },
            { "store": "Epic", "id": "fennec", "executableName": "Game.exe" }
        ]));
        assert_eq!(detect_in(&both, &e), Some(content));
    }
}
