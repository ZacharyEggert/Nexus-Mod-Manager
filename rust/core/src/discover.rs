//! Locate installed games from `discovery.stores`, in definition order (C#
//! `GameStoreInstallationPathDetector`): Steam libraries, GOG Galaxy and plain registry keys,
//! Epic `.item` manifests, Xbox / Microsoft Store folders. A hit must hold the store's
//! executable (C# `ValidatePath`).
//!
//! On Linux also: Flatpak / Snap Steam, Heroic (GOG + Epic via Legendary) and, for any game,
//! Lutris; `wine_user_dir` finds the Wine prefix Heroic or Lutris runs a Windows game in.

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
    /// Heroic config folders (`~/.config/heroic`): `gog_store/installed.json`, `GamesConfig/`.
    pub heroic: Vec<PathBuf>,
    /// Folders holding Legendary's `installed.json` (Heroic's Epic backend, or standalone).
    pub legendary: Vec<PathBuf>,
    /// Lutris game config folders (`*.yml`).
    pub lutris: Vec<PathBuf>,
    /// Wine prefixes / bottles (CrossOver, Whisky, Bottles, `~/.wine`), each searched like a
    /// Windows install of its own.
    pub wine_prefixes: Vec<PathBuf>,
    /// Set while searching inside a prefix: Windows paths (`C:\...`) map into it.
    pub prefix: Option<PathBuf>,
}

impl Env {
    /// A path a store reported, on this machine: `X:\...` inside `prefix` when searching a bottle.
    fn host_path(&self, raw: &str) -> PathBuf {
        let Some(prefix) = &self.prefix else {
            return PathBuf::from(raw);
        };
        let b = raw.as_bytes();
        if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
            let drive = (b[0] as char).to_ascii_lowercase();
            let base = if drive == 'c' {
                prefix.join("drive_c")
            } else {
                prefix.join("dosdevices").join(format!("{drive}:"))
            };
            return base.join(native(&raw[2..]));
        }
        PathBuf::from(raw)
    }

    /// The same sources, inside the Wine prefix `p`: its Steam, registry files and Epic manifests.
    fn in_prefix(p: &Path) -> Self {
        let c = p.join("drive_c");
        let prefix = p.to_path_buf();
        let reg_prefix = prefix.clone();
        Self {
            steam_roots: ["Program Files (x86)/Steam", "Program Files/Steam"]
                .iter()
                .map(|r| c.join(r))
                .filter(|r| r.is_dir())
                .collect(),
            registry: Box::new(move |k, v| wine_registry(&reg_prefix, k, v)),
            epic_manifests: Some(c.join("ProgramData/Epic/EpicGamesLauncher/Data/Manifests")),
            xbox_roots: vec![],
            heroic: vec![],
            legendary: vec![],
            lutris: vec![],
            wine_prefixes: vec![],
            prefix: Some(prefix),
        }
    }
}

/// A value from a Wine prefix's registry files (`system.reg` = HKLM, `user.reg` = HKCU).
fn wine_registry(prefix: &Path, key: &str, value: &str) -> Option<String> {
    let (hive, path) = key.split_once('\\')?;
    let file = match hive.to_ascii_uppercase().as_str() {
        "HKEY_LOCAL_MACHINE" | "HKLM" => "system.reg",
        "HKEY_CURRENT_USER" | "HKCU" => "user.reg",
        _ => return None,
    };
    let text = crate::ini::decode(&std::fs::read(prefix.join(file)).ok()?);
    let unescape = |s: &str| s.replace("\\\\", "\\");
    let mut inside = false;
    for line in text.lines() {
        if let Some(sec) = line.strip_prefix('[') {
            let name = sec.rsplit_once(']').map_or(sec, |(n, _)| n);
            inside = unescape(name).eq_ignore_ascii_case(path);
            continue;
        }
        if !inside {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if !k.trim_matches('"').eq_ignore_ascii_case(value) {
            continue;
        }
        if let Some(d) = v.strip_prefix("dword:") {
            return u32::from_str_radix(d.trim(), 16)
                .ok()
                .map(|n| n.to_string());
        }
        return Some(unescape(v.trim().trim_matches('"')));
    }
    None
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
        let (mut heroic, mut legendary, mut lutris) = (vec![], vec![], vec![]);
        if let Some(h) = dirs::home_dir() {
            let flatpak = |app: &str, rel: &str| h.join(".var/app").join(app).join(rel);
            steam_roots.extend([
                h.join(".steam/steam"),
                h.join(".local/share/Steam"),
                flatpak("com.valvesoftware.Steam", ".local/share/Steam"),
                flatpak("com.valvesoftware.Steam", "data/Steam"),
                h.join("snap/steam/common/.local/share/Steam"),
                h.join("Library/Application Support/Steam"),
            ]);
            heroic.extend([
                h.join(".config/heroic"),
                flatpak("com.heroicgameslauncher.hgl", "config/heroic"),
            ]);
            legendary.extend(heroic.iter().map(|d| d.join("legendaryConfig/legendary")));
            legendary.push(h.join(".config/legendary"));
            lutris.extend([
                h.join(".config/lutris/games"),
                h.join(".local/share/lutris/games"),
                flatpak("net.lutris.Lutris", "config/lutris/games"),
                flatpak("net.lutris.Lutris", "data/lutris/games"),
            ]);
        }
        steam_roots.retain(|p| p.is_dir());
        // ~/.steam/steam is usually a link to ~/.local/share/Steam.
        let mut seen = std::collections::HashSet::new();
        steam_roots.retain(|p| seen.insert(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())));
        for v in [&mut heroic, &mut legendary, &mut lutris] {
            v.retain(|p| p.is_dir());
        }
        let mut wine_prefixes = vec![];
        if let Some(h) = dirs::home_dir() {
            wine_prefixes.push(h.join(".wine"));
            for bottles in [
                h.join("Library/Application Support/CrossOver/Bottles"),
                h.join("Library/Containers/com.isaacmarovitz.Whisky/Bottles"),
                h.join(".local/share/bottles/bottles"),
                h.join(".var/app/com.usebottles.bottles/data/bottles/bottles"),
            ] {
                let mut found: Vec<PathBuf> = std::fs::read_dir(bottles)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .collect();
                found.sort();
                wine_prefixes.extend(found);
            }
        }
        wine_prefixes.retain(|p| p.join("drive_c").is_dir());
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
            heroic,
            legendary,
            lutris,
            wine_prefixes,
            prefix: None,
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
pub(crate) fn libraries(root: &Path, env: &Env) -> Vec<PathBuf> {
    let mut libs = vec![root.to_path_buf()];
    for vdf in [
        "steamapps/libraryfolders.vdf",
        "SteamApps/libraryfolders.vdf",
    ] {
        if let Ok(vdf) = std::fs::read_to_string(root.join(vdf)) {
            for line in vdf.lines() {
                let parts: Vec<&str> = line.split('"').collect();
                if parts.len() >= 4 && parts[1] == "path" {
                    let p = env.host_path(&parts[3].replace("\\\\", "\\"));
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
pub(crate) fn expand_env(s: &str) -> String {
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
fn validate(path: &str, s: &Store, env: &Env) -> Option<PathBuf> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let mut p = env.host_path(&expand_env(path));
    if let Some(x) = &s.path_suffix {
        p = tidy(&p.join(native(x)));
    }
    if !p.is_dir() {
        return None;
    }
    // Off Windows a Steam folder is the game's own (exact install folder name), and native
    // mac/Linux builds ship differently named binaries or an .app bundle.
    let trust_folder =
        !cfg!(windows) && env.prefix.is_none() && s.store.eq_ignore_ascii_case("steam");
    if let Some(exe) = s.executable_name.as_ref().filter(|_| !trust_folder) {
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
        .flat_map(|r| libraries(r, env))
        .find_map(|lib| {
            validate(
                &lib.join("steamapps/common").join(&folder).to_string_lossy(),
                s,
                env,
            )
        });
    in_library.or_else(|| {
        // Steam's own uninstall entry, for libraries libraryfolders.vdf doesn't list.
        let id = s.id.as_deref()?;
        let key = format!(
            r"HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Steam App {id}"
        );
        validate(&(env.registry)(&key, "InstallLocation")?, s, env)
    })
}

fn json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Heroic's installed GOG games: (appName, install_path).
fn heroic_gog(env: &Env) -> Vec<(String, String)> {
    env.heroic
        .iter()
        .filter_map(|h| json(&h.join("gog_store/installed.json")))
        .flat_map(|v| {
            v.get("installed")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|g| {
            Some((
                g.get("appName")?.as_str()?.to_string(),
                g.get("install_path")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// Legendary's installed Epic games: (app_name, title, install_path).
fn legendary_games(env: &Env) -> Vec<(String, String, String)> {
    env.legendary
        .iter()
        .filter_map(|d| json(&d.join("installed.json")))
        .flat_map(|v| v.as_object().cloned().unwrap_or_default().into_values())
        .filter_map(|g| {
            let f = |k: &str| g.get(k).and_then(|x| x.as_str()).map(String::from);
            Some((
                f("app_name")?,
                f("title").unwrap_or_default(),
                f("install_path")?,
            ))
        })
        .collect()
}

/// Lutris game configs: (exe, prefix) from each `game:` section.
// ponytail: line-based YAML read of `exe:` / `prefix:`; a real parser if Lutris configs get fancier.
fn lutris_games(env: &Env) -> Vec<(PathBuf, Option<PathBuf>)> {
    let mut out = vec![];
    for dir in &env.lutris {
        let mut ymls: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "yml"))
            .collect();
        ymls.sort();
        for y in ymls {
            let Ok(text) = std::fs::read_to_string(&y) else {
                continue;
            };
            let (mut exe, mut prefix, mut in_game) = (None, None, false);
            for line in text.lines() {
                if !line.starts_with([' ', '\t']) {
                    in_game = line.trim_end() == "game:";
                    continue;
                }
                let Some((k, v)) = line.trim().split_once(':') else {
                    continue;
                };
                let v = v.trim().trim_matches(['"', '\'']);
                match k {
                    "exe" if in_game && !v.is_empty() => exe = Some(PathBuf::from(v)),
                    "prefix" if in_game && !v.is_empty() => prefix = Some(PathBuf::from(v)),
                    _ => {}
                }
            }
            if let Some(exe) = exe {
                out.push((exe, prefix));
            }
        }
    }
    out
}

/// The game folder for `exe` when it is `<folder>/<rel>` for one of `rels` (case-insensitive).
fn folder_of(exe: &Path, rels: &[&str]) -> Option<PathBuf> {
    rels.iter().find_map(|rel| {
        let rel = native(rel);
        let n = rel.components().count();
        let comps: Vec<_> = exe.components().collect();
        let tail = comps.len().checked_sub(n)?;
        let same = comps[tail..]
            .iter()
            .zip(rel.components())
            .all(|(a, b)| a.as_os_str().eq_ignore_ascii_case(b.as_os_str()));
        same.then(|| comps[..tail].iter().collect())
    })
}

fn lutris(game: &GameDef, env: &Env) -> Option<PathBuf> {
    let rels: Vec<&str> = game
        .game_executables
        .iter()
        .map(String::as_str)
        .chain(
            game.discovery
                .stores
                .iter()
                .filter_map(|s| s.executable_name.as_deref()),
        )
        .collect();
    lutris_games(env)
        .into_iter()
        .find_map(|(exe, _)| folder_of(&exe, &rels).filter(|p| p.is_dir()))
}

/// The Windows user folder of the Wine prefix Heroic or Lutris runs the game at `game_path` in.
pub fn wine_user_dir(game_path: &Path) -> Option<PathBuf> {
    wine_user_dir_in(game_path, &Env::system())
}

pub fn wine_user_dir_in(game_path: &Path, env: &Env) -> Option<PathBuf> {
    let same = |p: &str| Path::new(p) == game_path;
    let apps: Vec<String> = heroic_gog(env)
        .into_iter()
        .filter(|(_, p)| same(p))
        .map(|(a, _)| a)
        .chain(
            legendary_games(env)
                .into_iter()
                .filter(|(_, _, p)| same(p))
                .map(|(a, _, _)| a),
        )
        .collect();
    let heroic = apps.iter().find_map(|app| {
        env.heroic.iter().find_map(|h| {
            let v = json(&h.join("GamesConfig").join(format!("{app}.json")))?;
            let prefix = v.get(app)?.get("winePrefix")?.as_str()?;
            crate::game::prefix_user(Path::new(prefix))
        })
    });
    heroic.or_else(|| {
        lutris_games(env)
            .into_iter()
            .filter(|(exe, _)| exe.starts_with(game_path))
            .find_map(|(_, prefix)| crate::game::prefix_user(&prefix?))
    })
}

fn gog(s: &Store, env: &Env) -> Option<PathBuf> {
    let heroic = s.id.as_deref().and_then(|id| {
        heroic_gog(env)
            .into_iter()
            .find(|(a, _)| a == id)
            .and_then(|(_, p)| validate(&p, s, env))
    });
    if heroic.is_some() {
        return heroic;
    }
    let value = s.registry_value_name.as_deref().unwrap_or("PATH");
    if let Some(k) = &s.registry_key {
        return validate(&(env.registry)(k, value)?, s, env);
    }
    let id = s.id.as_deref()?;
    [
        format!(r"HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\GOG.com\Games\{id}"),
        format!(r"HKEY_LOCAL_MACHINE\SOFTWARE\GOG.com\Games\{id}"),
        format!(r"HKEY_CURRENT_USER\SOFTWARE\GOG.com\Games\{id}"),
    ]
    .iter()
    .find_map(|k| validate(&(env.registry)(k, value)?, s, env))
}

fn registry(s: &Store, env: &Env) -> Option<PathBuf> {
    let value = s
        .registry_value_name
        .as_deref()
        .unwrap_or("InstallLocation");
    validate(&(env.registry)(s.registry_key.as_deref()?, value)?, s, env)
}

fn epic(s: &Store, env: &Env) -> Option<PathBuf> {
    let legendary = legendary_games(env)
        .into_iter()
        .find_map(|(app, title, path)| {
            let by_id =
                s.id.as_deref()
                    .is_some_and(|id| app.eq_ignore_ascii_case(id));
            let by_name = s
                .install_folder_name
                .as_deref()
                .is_some_and(|n| title.eq_ignore_ascii_case(n));
            (by_id || by_name)
                .then(|| validate(&path, s, env))
                .flatten()
        });
    if legendary.is_some() {
        return legendary;
    }
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
            .then(|| validate(field("InstallLocation")?, s, env))
            .flatten()
    })
}

fn xbox(s: &Store, env: &Env) -> Option<PathBuf> {
    if let Some(k) = &s.registry_key {
        let value = s
            .registry_value_name
            .as_deref()
            .unwrap_or("InstallLocation");
        if let Some(p) = (env.registry)(k, value).and_then(|p| validate(&p, s, env)) {
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
        cands
            .iter()
            .find_map(|c| validate(&c.to_string_lossy(), s, env))
    })
}

pub fn detect(game: &GameDef) -> Option<PathBuf> {
    detect_in(game, &Env::system())
}

/// This machine's stores, then Lutris, then each Wine prefix's own.
pub fn detect_in(game: &GameDef, env: &Env) -> Option<PathBuf> {
    let stores = |env: &Env| {
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
    };
    stores(env).or_else(|| lutris(game, env)).or_else(|| {
        env.wine_prefixes
            .iter()
            .find_map(|p| stores(&Env::in_prefix(p)))
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
            heroic: vec![],
            legendary: vec![],
            lutris: vec![],
            wine_prefixes: vec![],
            prefix: None,
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
        // Folder without the executable: not a hit on Windows; elsewhere Steam's folder is trusted
        // (native builds name their binaries differently).
        let data = lib.join("steamapps/common/Dark Souls Prepare to Die Edition/DATA");
        fs::create_dir_all(&data).unwrap();
        assert_eq!(detect_in(&ds, &e), (!cfg!(windows)).then(|| data.clone()));
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
    fn heroic_legendary_and_lutris() {
        let t = tempfile::tempdir().unwrap();
        let heroic = t.path().join("heroic");
        let sdv = t.path().join("Games/Heroic/Stardew Valley");
        touch(&sdv.join("Stardew Valley.exe"));
        fs::create_dir_all(heroic.join("gog_store")).unwrap();
        fs::write(
            heroic.join("gog_store/installed.json"),
            serde_json::json!({"installed": [{"appName": "1453375253", "install_path": sdv, "platform": "windows"}]}).to_string(),
        )
        .unwrap();
        let prefix = t.path().join("Prefixes/default/Stardew Valley");
        fs::create_dir_all(prefix.join("drive_c/users/Public")).unwrap();
        fs::create_dir_all(prefix.join("drive_c/users/someone")).unwrap();
        fs::create_dir_all(heroic.join("GamesConfig")).unwrap();
        fs::write(
            heroic.join("GamesConfig/1453375253.json"),
            serde_json::json!({"1453375253": {"winePrefix": prefix}}).to_string(),
        )
        .unwrap();
        let mut e = env(&[]);
        e.heroic = vec![heroic];
        let sdv_game = crate::game::find("stardewvalley").unwrap();
        assert_eq!(detect_in(&sdv_game, &e), Some(sdv.clone()));
        assert_eq!(
            wine_user_dir_in(&sdv, &e),
            Some(prefix.join("drive_c/users/someone"))
        );

        // Epic through Legendary, by app name.
        let leg = t.path().join("legendary");
        let cp = t.path().join("Games/Heroic/Cyberpunk 2077");
        touch(&cp.join("bin/x64/Cyberpunk2077.exe"));
        fs::create_dir_all(&leg).unwrap();
        fs::write(
            leg.join("installed.json"),
            serde_json::json!({"Ginger": {"app_name": "Ginger", "title": "Fancy", "install_path": cp}}).to_string(),
        )
        .unwrap();
        e.legendary = vec![leg];
        let epic = game(
            serde_json::json!([{ "store": "Epic", "id": "ginger", "executableName": "bin\\x64\\Cyberpunk2077.exe" }]),
        );
        assert_eq!(detect_in(&epic, &e), Some(cp));

        // Lutris: any game, matched by executable; its prefix holds the user folders.
        let lut = t.path().join("lutris");
        fs::create_dir_all(&lut).unwrap();
        let pfx = t.path().join("Games/skyrim");
        let sse =
            pfx.join("drive_c/Program Files (x86)/Steam/steamapps/common/Skyrim Special Edition");
        touch(&sse.join("SkyrimSE.exe"));
        fs::create_dir_all(pfx.join("drive_c/users/steamuser")).unwrap();
        fs::write(
            lut.join("skyrim-1.yml"),
            format!(
                "game:\n  exe: {}\n  prefix: {}\nsystem:\n  exe: nope\n",
                sse.join("SkyrimSE.exe").display(),
                pfx.display()
            ),
        )
        .unwrap();
        e.lutris = vec![lut];
        let sky = crate::game::find("SkyrimSE").unwrap();
        assert_eq!(detect_in(&sky, &e), Some(sse.clone()));
        assert_eq!(
            wine_user_dir_in(&sse, &e),
            Some(pfx.join("drive_c/users/steamuser"))
        );
        // A game folder inside drive_c resolves its prefix without any launcher config.
        if cfg!(not(windows)) {
            assert_eq!(
                sky.windows_user_dir(&sse),
                Some(pfx.join("drive_c/users/steamuser"))
            );
            assert_eq!(
                sky.plugins_file(&sse).unwrap(),
                pfx.join(
                    "drive_c/users/steamuser/AppData/Local/Skyrim Special Edition/plugins.txt"
                )
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn wine_bottles() {
        let t = tempfile::tempdir().unwrap();
        // CrossOver-style bottle with Steam for Windows and a second library on D:.
        let bottle = t.path().join("Bottles/Steam");
        let c = bottle.join("drive_c");
        let steam = c.join("Program Files (x86)/Steam");
        fs::create_dir_all(steam.join("steamapps")).unwrap();
        fs::write(
            steam.join("steamapps/libraryfolders.vdf"),
            "\"libraryfolders\"\n{\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}\n",
        )
        .unwrap();
        let d = t.path().join("ext");
        fs::create_dir_all(bottle.join("dosdevices")).unwrap();
        std::os::unix::fs::symlink(&d, bottle.join("dosdevices/d:")).unwrap();
        let sse = d.join("SteamLibrary/steamapps/common/Skyrim Special Edition");
        touch(&sse.join("SkyrimSE.exe"));
        fs::create_dir_all(c.join("users/crossover")).unwrap();
        let mut e = env(&[]);
        e.wine_prefixes = vec![bottle.clone()];
        let sky = crate::game::find("SkyrimSE").unwrap();
        let found = detect_in(&sky, &e).unwrap();
        assert_eq!(
            found,
            bottle.join("dosdevices/d:/SteamLibrary/steamapps/common/Skyrim Special Edition")
        );
        // In a bottle the executable must be there (Windows rules).
        fs::remove_file(sse.join("SkyrimSE.exe")).unwrap();
        assert_eq!(detect_in(&sky, &e), None);

        // GOG Galaxy in a bottle: its system.reg holds the key, C:\ maps to drive_c.
        let gog_dir = c.join("GOG Games/Stardew Valley");
        touch(&gog_dir.join("Stardew Valley.exe"));
        fs::write(
            bottle.join("system.reg"),
            "WINE REGISTRY Version 2\n\n[Software\\\\Wow6432Node\\\\GOG.com\\\\Games\\\\1453375253] 1700000000\n#time=1d\n\"PATH\"=\"C:\\\\GOG Games\\\\Stardew Valley\"\n\"x\"=dword:0000000a\n",
        )
        .unwrap();
        assert_eq!(
            wine_registry(
                &bottle,
                r"HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\GOG.com\Games\1453375253",
                "x"
            )
            .as_deref(),
            Some("10")
        );
        let sdv = crate::game::find("stardewvalley").unwrap();
        let found = detect_in(&sdv, &e).unwrap();
        assert_eq!(found, gog_dir);
        // Its settings live in the bottle's user folder.
        assert_eq!(
            sdv.windows_user_dir(&found),
            Some(c.join("users/crossover"))
        );
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
