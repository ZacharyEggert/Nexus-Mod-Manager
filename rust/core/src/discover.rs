//! Locate installed games via Steam library folders.

use crate::game::GameDef;
use std::path::{Path, PathBuf};

fn steam_roots() -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from(r"C:\Program Files (x86)\Steam"),
        PathBuf::from(r"C:\Program Files\Steam"),
    ];
    if let Some(h) = dirs::home_dir() {
        v.push(h.join(".steam/steam"));
        v.push(h.join(".local/share/Steam"));
        v.push(h.join("Library/Application Support/Steam"));
    }
    v.retain(|p| p.is_dir());
    v
}

/// Library roots listed in `libraryfolders.vdf` (`"path"  "D:\\SteamLibrary"` lines).
fn libraries(root: &Path) -> Vec<PathBuf> {
    let mut libs = vec![root.to_path_buf()];
    if let Ok(vdf) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
        for line in vdf.lines() {
            let parts: Vec<&str> = line.split('"').collect();
            if parts.len() >= 4 && parts[1] == "path" {
                libs.push(PathBuf::from(parts[3].replace("\\\\", "\\")));
            }
        }
    }
    libs
}

/// A definition's relative Windows path (`Zenimax Online\\The Elder Scrolls Online`) as a native one.
fn native(rel: &str) -> PathBuf {
    rel.split(['\\', '/']).filter(|c| !c.is_empty()).collect()
}

// ponytail: Steam only; GOG/Epic/Xbox need registry or manifest probing, add when users ask.
pub fn detect(game: &GameDef) -> Option<PathBuf> {
    let folders: Vec<PathBuf> = game
        .discovery
        .stores
        .iter()
        .filter(|s| s.store == "Steam")
        .filter_map(|s| {
            let f = native(s.install_folder_name.as_deref()?);
            Some(match &s.path_suffix {
                Some(x) => f.join(native(x)),
                None => f,
            })
        })
        .collect();
    steam_roots()
        .iter()
        .flat_map(|r| libraries(r))
        .flat_map(|lib| {
            folders
                .iter()
                .map(move |f| lib.join("steamapps/common").join(f))
        })
        .find(|p| p.is_dir())
}
