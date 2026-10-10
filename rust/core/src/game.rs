//! Data-driven game modes, read straight from the C# `Game Modes/Definitions/*/game.json`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

macro_rules! defs {
    ($($id:literal),*) => {
        &[$(include_str!(concat!("../../../Game Modes/Definitions/", $id, "/game.json"))),*]
    };
}

// ponytail: only the 17 data-driven modes; the C#-coded ones (BG3, DarkSouls, ...) need a game.json first.
const DEFINITIONS: &[&str] = defs!(
    "Cyberpunk2077", "Fallout3", "Fallout4", "FalloutNV", "Grimrock", "NoMansSky", "Oblivion",
    "OblivionRemastered", "Skyrim", "SkyrimGOG", "SkyrimSE", "Starfield", "Subnautica",
    "SubnauticaBelowZero", "TheSims4", "XCOM2", "stardewvalley"
);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameDef {
    pub mode_id: String,
    pub name: String,
    #[serde(default)]
    pub game_executables: Vec<String>,
    #[serde(default)]
    pub stop_folders: Vec<String>,
    #[serde(default)]
    pub plugin_extensions: Vec<String>,
    #[serde(default)]
    pub discovery: Discovery,
    pub plugin: Option<PluginDef>,
    #[serde(default)]
    pub mod_install: ModInstallDef,
    pub launcher: Option<LauncherDef>,
    pub gamebryo: Option<GamebryoDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GamebryoDef {
    pub plugins_file_path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Discovery {
    #[serde(default)]
    pub stores: Vec<Store>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Store {
    pub store: String,
    pub id: Option<String>,
    pub install_folder_name: Option<String>,
    pub executable_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDef {
    #[serde(default)]
    pub uses_plugins: bool,
    pub plugin_directory_suffix: Option<String>,
    pub policy: Option<PluginPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginPolicy {
    pub plugins_file_path: Option<String>,
    pub active_marker: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModInstallDef {
    #[serde(default)]
    pub hardlink_required_extensions: Vec<String>,
    #[serde(default)]
    pub real_file_required_extensions: Vec<String>,
    pub managed_installation_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherDef {
    pub default_executable: Option<String>,
}

pub fn all() -> Vec<GameDef> {
    DEFINITIONS
        .iter()
        .map(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).expect("bundled game.json is valid"))
        .collect()
}

pub fn find(mode_id: &str) -> Option<GameDef> {
    all().into_iter().find(|g| g.mode_id.eq_ignore_ascii_case(mode_id))
}

/// Expand `{GamePath}`-style tokens and Windows separators into a native path.
pub fn expand(template: &str, game_path: &Path) -> PathBuf {
    let tokens: [(&str, Option<PathBuf>); 6] = [
        ("{GamePath}", Some(game_path.to_path_buf())),
        ("{LocalApplicationData}", dirs::data_local_dir()),
        ("{PersonalData}", dirs::document_dir()),
        ("{Documents}", dirs::document_dir()),
        ("{ProgramFiles}", Some(PathBuf::from(r"C:\Program Files"))),
        ("{UserGameData}", None),
    ];
    let mut s = template.to_string();
    for (t, v) in tokens {
        if let Some(v) = v {
            s = s.replace(t, &v.to_string_lossy());
        }
    }
    let native = s.replace('\\', std::path::MAIN_SEPARATOR_STR);
    let p = PathBuf::from(native);
    if p.is_absolute() { p } else { game_path.join(p) }
}

impl GameDef {
    /// Folder mod files are deployed relative to (Data for gamebryo, managed path, else game root).
    pub fn install_root(&self, game_path: &Path) -> PathBuf {
        if let Some(m) = &self.mod_install.managed_installation_path {
            return expand(m, game_path);
        }
        match self.plugin.as_ref().and_then(|p| p.plugin_directory_suffix.as_deref()) {
            Some(suffix) => expand(suffix, game_path),
            None => game_path.to_path_buf(),
        }
    }

    pub fn plugins_file(&self, game_path: &Path) -> Option<PathBuf> {
        let p = self.plugin.as_ref()?;
        if !p.uses_plugins {
            return None;
        }
        let path = p
            .policy
            .as_ref()
            .and_then(|p| p.plugins_file_path.as_deref())
            .or(self.gamebryo.as_ref().and_then(|g| g.plugins_file_path.as_deref()))?;
        Some(expand(path, game_path))
    }

    pub fn active_marker(&self) -> &str {
        self.plugin
            .as_ref()
            .and_then(|p| p.policy.as_ref())
            .and_then(|p| p.active_marker.as_deref())
            .unwrap_or("")
    }

    pub fn is_plugin(&self, file: &str) -> bool {
        has_ext(file, &self.plugin_extensions)
    }

    pub fn needs_real_copy(&self, file: &str) -> bool {
        has_ext(file, &self.mod_install.real_file_required_extensions)
    }

    pub fn executable(&self, game_path: &Path) -> Option<PathBuf> {
        let exe = self
            .launcher
            .as_ref()
            .and_then(|l| l.default_executable.clone())
            .or_else(|| self.game_executables.first().cloned())?;
        Some(expand(&exe, game_path))
    }
}

pub(crate) fn has_ext(file: &str, exts: &[String]) -> bool {
    let lower = file.to_ascii_lowercase();
    exts.iter().any(|e| lower.ends_with(&e.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn all_definitions_parse() {
        let all = super::all();
        assert_eq!(all.len(), 17);
        let sse = super::find("skyrimse").unwrap();
        assert!(sse.stop_folders.iter().any(|f| f == "SKSE"));
        assert_eq!(sse.active_marker(), "*");
    }
}
