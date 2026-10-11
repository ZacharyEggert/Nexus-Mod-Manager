//! Data-driven game modes, read straight from the C# `Game Modes/Definitions/*/game.json`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Bundled game.json + Categories.xml for each data-driven mode, index-aligned.
macro_rules! games {
    ($($id:literal),*) => {
        const DEFINITIONS: &[&str] =
            &[$(include_str!(concat!("../../../Game Modes/Definitions/", $id, "/game.json"))),*];
        const CATEGORIES: &[&str] =
            &[$(include_str!(concat!("../../../Game Modes/Definitions/", $id, "/Categories.xml"))),*];
    };
}

games!(
    "BaldursGate3",
    "BreakingWheel",
    "Cyberpunk2077",
    "DarkSouls",
    "DarkSouls2",
    "DragonAge",
    "DragonAge2",
    "DragonsDogma",
    "Enderal",
    "EnderalSE",
    "Fallout3",
    "Fallout4",
    "Fallout4VR",
    "FalloutNV",
    "Grimrock",
    "HogwartsLegacy",
    "MonsterHunterWorld",
    "Morrowind",
    "MountAndBlade2Bannerlord",
    "NoMansSky",
    "Oblivion",
    "OblivionRemastered",
    "Skyrim",
    "SkyrimGOG",
    "SkyrimSE",
    "SkyrimVR",
    "Starbound",
    "Starfield",
    "StateOfDecay",
    "Subnautica",
    "SubnauticaBelowZero",
    "TESO",
    "TheSims4",
    "WarThunder",
    "Witcher2",
    "Witcher3",
    "WorldOfTanks",
    "XCOM2",
    "XRebirth",
    "stardewvalley"
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
    /// Base game masters: always active, pinned at the top in this order.
    #[serde(default)]
    pub ordered_critical_plugin_names: Vec<String>,
    /// DLC: fixed order right after the critical plugins.
    #[serde(default)]
    pub ordered_official_plugin_names: Vec<String>,
    /// Official plugins the game loads on its own (Creation Club); also read from `.ccc` files.
    #[serde(default)]
    pub ordered_official_unmanaged_plugin_names: Vec<String>,
    /// Files the game mode can't run without (C# refuses to open the game); relative to the plugin
    /// folder, else the game folder.
    pub critical_files_error_message: Option<String>,
    /// An external tool the mods install into (Dark Souls 2: GeDoSaTo); its folder is `{ToolFolder}`.
    pub required_tool_name: Option<String>,
    /// Files that must exist under the tool folder.
    #[serde(default)]
    pub ordered_required_tool_file_names: Vec<String>,
    pub required_tool_error_message: Option<String>,
    /// Runtime: the user's folder for `required_tool_name`.
    #[serde(skip)]
    pub tool_folder: Option<PathBuf>,
    /// Runtime: Windows user folder (`.../users/<name>`) to resolve `{PersonalData}` etc. in, instead
    /// of this OS's or the Proton prefix's. Tests point it at a temp dir.
    #[serde(skip)]
    pub user_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GamebryoDef {
    pub plugins_file_path: Option<String>,
    /// `oblivion`, `fallout3`, `falloutnv`: which archive invalidation the game needs.
    pub archive_invalidation_profile: Option<String>,
    /// `{UserGameData}`: the game's My Games folder.
    pub user_game_data_path: Option<String>,
    pub ini_file_path: Option<String>,
    /// Other INI files by C# settings key (`FODefaultIniPath`).
    #[serde(default)]
    pub additional_settings_files: std::collections::BTreeMap<String, String>,
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
    /// Subfolder of the found folder that is the game folder (Dark Souls: `DATA`).
    pub path_suffix: Option<String>,
    pub registry_key: Option<String>,
    pub registry_value_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDef {
    #[serde(default)]
    pub uses_plugins: bool,
    pub plugin_directory_suffix: Option<String>,
    pub policy: Option<PluginPolicy>,
    /// 0 = no limit.
    #[serde(default)]
    pub max_allowed_active_plugins_count: usize,
    /// Files listing official unmanaged plugins, one per line (`{GamePath}\Skyrim.ccc`).
    #[serde(default)]
    pub official_unmanaged_plugin_list_files: Vec<String>,
}

/// `plugin.policy`: how the game persists load order (C# `PluginManagementPolicy`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginPolicy {
    pub persistence_strategy: Option<String>,
    pub plugins_file_path: Option<String>,
    pub load_order_file_path: Option<String>,
    pub active_marker: Option<String>,
    pub use_timestamp_order: Option<bool>,
    pub ignore_official_plugins: Option<bool>,
    pub single_file_management: Option<bool>,
    pub official_plugins_are_implicitly_active: Option<bool>,
    pub load_order_in_plugin_directory: Option<bool>,
    pub show_starfield_custom_plugins_header: Option<bool>,
    #[serde(default)]
    pub official_plugins: Vec<String>,
    #[serde(default)]
    pub fixed_order_plugins: Vec<String>,
    #[serde(default)]
    pub forced_active_plugins: Vec<String>,
    pub master_plugins_must_load_before_non_masters: Option<bool>,
    pub validate_dependencies: Option<bool>,
    #[serde(default)]
    pub address_spaces: Vec<AddressSpaceDef>,
    #[serde(default)]
    pub blueprint_plugins: Vec<String>,
    #[serde(default)]
    pub blueprint_plugin_prefixes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressSpaceDef {
    /// `Full`, `Light`, `Medium` or `Small` (Starfield's name for light).
    pub address_class: String,
    pub first_index: usize,
    pub max_count: usize,
    pub display_format: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModInstallDef {
    #[serde(default)]
    pub hardlink_required_extensions: Vec<String>,
    #[serde(default)]
    pub real_file_required_extensions: Vec<String>,
    pub managed_installation_path: Option<String>,
    #[serde(default)]
    pub path_adjustment_profile: crate::adjust::Profile,
    /// Archive files beside the Data folder (script extender exe/dll) may deploy to the game root.
    #[serde(default)]
    pub supports_game_root_install: bool,
    /// Where `Dest::GameRoot` files go instead of the game folder (C# `SecondaryInstallationPath`):
    /// Dragon Age `.dazip` content, Witcher 2 user content.
    pub secondary_installation_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherDef {
    pub default_executable: Option<String>,
}

pub fn all() -> Vec<GameDef> {
    DEFINITIONS
        .iter()
        .map(|s| {
            serde_json::from_str(s.trim_start_matches('\u{feff}'))
                .expect("bundled game.json is valid")
        })
        .collect()
}

pub fn find(mode_id: &str) -> Option<GameDef> {
    all()
        .into_iter()
        .find(|g| g.mode_id.eq_ignore_ascii_case(mode_id))
}

/// Expand `{GamePath}`-style tokens and Windows separators into a native path.
pub fn expand(template: &str, game_path: &Path) -> PathBuf {
    expand_in(template, game_path, None, &[])
}

/// `expand`, with the Windows user folders taken from `user` (a Wine/Proton prefix's
/// `drive_c/users/<name>`) instead of this OS's, plus game-specific `extra` tokens.
fn expand_in(
    template: &str,
    game_path: &Path,
    user: Option<&Path>,
    extra: &[(&str, Option<PathBuf>)],
) -> PathBuf {
    let (local, docs) = match user {
        Some(u) => (Some(u.join("AppData/Local")), Some(u.join("Documents"))),
        None => (dirs::data_local_dir(), dirs::document_dir()),
    };
    let tokens: [(&str, Option<PathBuf>); 6] = [
        ("{GamePath}", Some(game_path.to_path_buf())),
        ("{LocalApplicationData}", local),
        ("{PersonalData}", docs.clone()),
        ("{Documents}", docs),
        ("{ProgramFiles}", Some(PathBuf::from(r"C:\Program Files"))),
        ("{UserGameData}", None),
    ];
    let mut s = template.to_string();
    for (t, v) in tokens.iter().chain(extra) {
        if let Some(v) = v {
            s = s.replace(t, &v.to_string_lossy());
        }
    }
    let native = s.replace('\\', std::path::MAIN_SEPARATOR_STR);
    let p = PathBuf::from(native);
    if p.is_absolute() {
        p
    } else {
        game_path.join(p)
    }
}

/// World of Tanks `res_mods/<version>`: `<version>` in `version.xml`, `\d+\.\d+\.\w+` (C# `ReadVersion`).
fn wot_version(game_path: &Path) -> String {
    let read = || -> Option<String> {
        let xml = std::fs::read_to_string(game_path.join("version.xml")).ok()?;
        let doc = roxmltree::Document::parse(&xml).ok()?;
        let v = doc
            .descendants()
            .find(|n| n.has_tag_name("version"))?
            .text()?;
        // ponytail: hand-rolled regex \d+\.\d+\.\w+ (no regex crate).
        let start = v.find(|c: char| c.is_ascii_digit())?;
        let rest = &v[start..];
        let mut parts = rest.splitn(3, '.');
        let (a, b, c) = (parts.next()?, parts.next()?, parts.next()?);
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let word: String = c
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        (digits(a) && digits(b) && !word.is_empty()).then(|| format!("{a}.{b}.{word}"))
    };
    read().unwrap_or_else(|| "0.0.0.0".into())
}

/// TESO's Documents folder per server: `liveeu` for the EU client folder, else `live`.
fn eso_server(game_path: &Path) -> &'static str {
    let eu = game_path
        .ancestors()
        .filter_map(|a| a.file_name())
        .any(|n| n.eq_ignore_ascii_case("The Elder Scrolls Online EU"));
    if eu {
        "liveeu"
    } else {
        "live"
    }
}

impl GameDef {
    /// Folder mod files are deployed relative to (Data for gamebryo, managed path, else game root).
    pub fn install_root(&self, game_path: &Path) -> PathBuf {
        if let Some(m) = &self.mod_install.managed_installation_path {
            return self.path(m, game_path);
        }
        match self
            .plugin
            .as_ref()
            .and_then(|p| p.plugin_directory_suffix.as_deref())
        {
            Some(suffix) => expand(suffix, game_path),
            None => game_path.to_path_buf(),
        }
    }

    /// Last component of the plugin dir (`Data`): the archive folder that maps onto the install root.
    pub fn data_dir_name(&self) -> Option<&str> {
        let s = self.plugin.as_ref()?.plugin_directory_suffix.as_deref()?;
        s.rsplit(['\\', '/']).next()
    }

    /// The Proton prefix's Windows user folder, for a Windows game run through Steam Play:
    /// `<library>/steamapps/compatdata/<appid>/pfx/drive_c/users/steamuser`. None on Windows,
    /// for native games, or before the game's first launch created the prefix.
    pub fn windows_user_dir(&self, game_path: &Path) -> Option<PathBuf> {
        if self.user_dir.is_some() {
            return self.user_dir.clone();
        }
        if cfg!(windows) {
            return None;
        }
        let steamapps = game_path.ancestors().find(|a| {
            a.file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("steamapps"))
        })?;
        self.discovery
            .stores
            .iter()
            .filter(|s| s.store == "Steam")
            .filter_map(|s| s.id.as_deref())
            .map(|id| {
                steamapps
                    .join("compatdata")
                    .join(id)
                    .join("pfx/drive_c/users/steamuser")
            })
            .find(|p| p.is_dir())
    }

    /// `expand` for this game: Windows user folders resolve inside its Proton prefix when it has one.
    pub fn path(&self, template: &str, game_path: &Path) -> PathBuf {
        let lazy = |t: &str, f: &dyn Fn() -> PathBuf| template.contains(t).then(f);
        let extra = [
            ("{ToolFolder}", self.tool_folder.clone()),
            (
                "{GameVersion}",
                lazy("{GameVersion}", &|| wot_version(game_path).into()),
            ),
            (
                "{EsoServer}",
                lazy("{EsoServer}", &|| eso_server(game_path).into()),
            ),
        ];
        expand_in(
            template,
            game_path,
            self.windows_user_dir(game_path).as_deref(),
            &extra,
        )
    }

    /// Where game-root files deploy: `secondaryInstallationPath`, else the game folder.
    pub fn secondary_root(&self, game_path: &Path) -> PathBuf {
        match &self.mod_install.secondary_installation_path {
            Some(t) => self.path(t, game_path),
            None => game_path.to_path_buf(),
        }
    }

    /// Why the game can't be managed yet (C# start-up checks): a missing critical file, or the
    /// required tool's folder unset / incomplete. None when everything is there.
    pub fn missing_files(&self, game_path: &Path) -> Option<String> {
        if let Some(tool) = &self.required_tool_name {
            let msg = || {
                self.required_tool_error_message
                    .clone()
                    .unwrap_or_else(|| format!("Set the {tool} folder to manage this game."))
            };
            let Some(dir) = &self.tool_folder else {
                return Some(msg());
            };
            if let Some(f) = self
                .ordered_required_tool_file_names
                .iter()
                .find(|f| !expand(f, dir).exists())
            {
                return Some(format!(
                    "You are missing {f} in the {tool} folder.\n{}",
                    msg()
                ));
            }
        }
        let base = self
            .plugin_dir(game_path)
            .unwrap_or_else(|| game_path.to_path_buf());
        let missing = self
            .ordered_critical_plugin_names
            .iter()
            .find(|f| !crate::manager::exists_ci(&base, f))?;
        Some(match &self.critical_files_error_message {
            Some(m) => format!("You are missing {missing}.\n{m}"),
            None => format!(
                "You are missing {missing}. Please verify your game install and ensure {missing} is present."
            ),
        })
    }

    /// `{UserGameData}`-aware `path` for gamebryo settings (`{UserGameData}\Oblivion.ini`).
    pub fn settings_path(&self, template: &str, game_path: &Path) -> PathBuf {
        let user = self
            .gamebryo
            .as_ref()
            .and_then(|g| g.user_game_data_path.as_deref())
            .map(|u| self.path(u, game_path))
            .unwrap_or_else(|| game_path.to_path_buf());
        self.path(
            &template.replace("{UserGameData}", &user.to_string_lossy()),
            game_path,
        )
    }

    /// Folder the game loads plugins from (`Data`).
    pub fn plugin_dir(&self, game_path: &Path) -> Option<PathBuf> {
        let suffix = self.plugin.as_ref()?.plugin_directory_suffix.as_deref()?;
        Some(expand(suffix, game_path))
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
            .or(self
                .gamebryo
                .as_ref()
                .and_then(|g| g.plugins_file_path.as_deref()))?;
        Some(self.path(path, game_path))
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

    /// Files the game won't read through a symlink; symlink mode hardlinks these instead.
    pub fn needs_hardlink(&self, file: &str) -> bool {
        has_ext(file, &self.mod_install.hardlink_required_extensions)
    }

    pub fn needs_real_copy(&self, file: &str) -> bool {
        has_ext(file, &self.mod_install.real_file_required_extensions)
    }

    /// Nexus Mods game domain (`skyrimspecialedition`).
    pub fn nexus_domain(&self) -> String {
        nexus_domain(&self.mode_id)
    }

    pub fn default_executable(&self) -> Option<&str> {
        self.launcher
            .as_ref()
            .and_then(|l| l.default_executable.as_deref())
            .or(self.game_executables.first().map(String::as_str))
    }

    /// What Launch runs: the user's custom command (relative = in the game folder), else the definition's.
    pub fn executable(&self, game_path: &Path, custom: Option<&str>) -> Option<PathBuf> {
        let exe = expand(custom.or(self.default_executable())?, game_path);
        // Definitions name the Windows .exe; native mac/Linux builds ship the same name without it.
        if cfg!(not(windows)) && !exe.exists() && has_ext(&exe.to_string_lossy(), &[".exe".into()])
        {
            let native = exe.with_extension("");
            if native.exists() {
                return Some(native);
            }
        }
        Some(exe)
    }
}

/// Nexus categories shipped with the mode (C# `Categories.xml`), used until the API's are fetched.
/// "Unassigned" (id 0) is left out: no category is `None`.
pub fn default_categories(mode_id: &str) -> Vec<crate::nexus::Category> {
    let Some(i) = all()
        .iter()
        .position(|g| g.mode_id.eq_ignore_ascii_case(mode_id))
    else {
        return vec![];
    };
    let xml = CATEGORIES[i].trim_start_matches('\u{feff}');
    let doc = roxmltree::Document::parse(xml).expect("bundled Categories.xml is valid");
    doc.descendants()
        .filter(|n| n.has_tag_name("category"))
        .filter_map(|n| {
            let id = n.attribute("ID")?.parse().ok().filter(|&id| id > 0)?;
            let name = n
                .children()
                .find(|c| c.has_tag_name("name"))
                .and_then(|c| c.text())
                .or(n.attribute("path"))?;
            Some(crate::nexus::Category {
                id,
                name: name.trim().into(),
                parent: None,
            })
        })
        .collect()
}

/// Canonical Nexus domain for a mode id or legacy domain, per C# `GameDomainTranslator`.
pub fn nexus_domain(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "skyrimse" | "skyrimgog" | "skyrimvr" => "skyrimspecialedition".into(),
        "falloutnv" => "newvegas".into(),
        "fallout4vr" => "fallout4".into(),
        "enderalse" => "enderalspecialedition".into(),
        // ponytail: C# used the mode id `teso`, which isn't a Nexus domain.
        "teso" => "elderscrollsonline".into(),
        m => m.into(),
    }
}

/// Game modes a Nexus link's domain belongs to (SkyrimSE + SkyrimGOG share one).
pub fn modes_for_domain(domain: &str) -> Vec<String> {
    let d = nexus_domain(domain);
    all()
        .into_iter()
        .filter(|g| g.nexus_domain() == d)
        .map(|g| g.mode_id)
        .collect()
}

pub(crate) fn has_ext(file: &str, exts: &[String]) -> bool {
    let lower = file.to_ascii_lowercase();
    exts.iter()
        .any(|e| lower.ends_with(&e.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn all_definitions_parse() {
        let all = super::all();
        assert_eq!(all.len(), 40);
        let sse = super::find("skyrimse").unwrap();
        assert!(sse.stop_folders.iter().any(|f| f == "SKSE"));
        assert_eq!(sse.active_marker(), "*");
        assert!(sse.mod_install.supports_game_root_install);
        assert_eq!(sse.data_dir_name(), Some("Data"));
        assert_eq!(sse.nexus_domain(), "skyrimspecialedition");
        assert_eq!(super::find("FalloutNV").unwrap().nexus_domain(), "newvegas");
        assert_eq!(
            super::modes_for_domain("skyrimse"),
            ["SkyrimGOG", "SkyrimSE", "SkyrimVR"]
        );
        assert_eq!(super::modes_for_domain("NewVegas"), ["FalloutNV"]);
        assert_eq!(super::modes_for_domain("morrowind"), ["Morrowind"]);
        assert_eq!(
            super::modes_for_domain("skyrimspecialedition"),
            ["SkyrimGOG", "SkyrimSE", "SkyrimVR"]
        );
        assert_eq!(
            super::find("EnderalSE").unwrap().nexus_domain(),
            "enderalspecialedition"
        );
        assert_eq!(
            super::find("TESO").unwrap().nexus_domain(),
            "elderscrollsonline"
        );
        assert_eq!(super::find("Witcher3").unwrap().nexus_domain(), "witcher3");
        // Every bundled Categories.xml parses; SkyrimSE has the categories Nexus uses (42 = User Interface).
        for g in &all {
            let _ = super::default_categories(&g.mode_id);
        }
        let cats = super::default_categories("SkyrimSE");
        assert!(
            cats.iter()
                .any(|c| c.id == 42 && c.name == "User Interface"),
            "{cats:?}"
        );
        assert!(cats.iter().all(|c| c.id != 0));
    }

    #[test]
    fn install_roots_and_tokens() {
        let t = tempfile::tempdir().unwrap();
        let user = t.path().join("user");
        let find = |m: &str| {
            let mut g = super::find(m).unwrap();
            g.user_dir = Some(user.clone());
            g
        };
        let game = t.path().join("The Elder Scrolls Online EU");
        assert_eq!(
            find("TESO").install_root(&game),
            user.join("Documents/Elder Scrolls Online/liveeu/Addons")
        );
        assert_eq!(
            find("TESO").install_root(&t.path().join("The Elder Scrolls Online")),
            user.join("Documents/Elder Scrolls Online/live/Addons")
        );
        let wot = t.path().join("wot");
        std::fs::create_dir_all(&wot).unwrap();
        assert_eq!(
            find("WorldOfTanks").install_root(&wot),
            wot.join("res_mods/0.0.0.0")
        );
        std::fs::write(
            wot.join("version.xml"),
            "<root><version> v.1.24.1.0 #1234</version></root>",
        )
        .unwrap();
        assert_eq!(
            find("WorldOfTanks").install_root(&wot),
            wot.join("res_mods/1.24.1")
        );
        let mut ds2 = find("DarkSouls2");
        ds2.tool_folder = Some(t.path().join("tools"));
        assert_eq!(
            ds2.install_root(&wot),
            t.path().join("tools/textures/DarkSoulsII")
        );
        assert_eq!(
            find("BaldursGate3").install_root(&wot),
            user.join("AppData/Local/Larian Studios/Baldur's Gate 3/Mods")
        );
        assert_eq!(find("BaldursGate3").secondary_root(&wot), wot);
        assert_eq!(
            find("DragonAge").secondary_root(&wot),
            user.join("Documents/BioWare/Dragon Age")
        );
        assert_eq!(find("Morrowind").install_root(&wot), wot.join("Data Files"));
    }

    #[test]
    fn missing_critical_files_and_tools() {
        let t = tempfile::tempdir().unwrap();
        let ds = super::find("DarkSouls").unwrap();
        let msg = ds.missing_files(t.path()).unwrap();
        assert!(
            msg.starts_with("You are missing DINPUT8.dll.") && msg.contains("DSfix"),
            "{msg}"
        );
        std::fs::write(t.path().join("dinput8.dll"), "").unwrap();
        std::fs::write(t.path().join("DSfix.ini"), "").unwrap();
        assert_eq!(ds.missing_files(t.path()), None);

        let mut ds2 = super::find("DarkSouls2").unwrap();
        assert!(ds2
            .missing_files(t.path())
            .unwrap()
            .contains("GeDoSaTo tool folder"));
        ds2.tool_folder = Some(t.path().into());
        assert!(ds2
            .missing_files(t.path())
            .unwrap()
            .contains("GeDoSaTo\\GeDoSaToTool.exe"));
        std::fs::create_dir_all(t.path().join("GeDoSaTo")).unwrap();
        for f in ["GeDoSaToTool.exe", "GeDoSaTo.dll"] {
            std::fs::write(t.path().join("GeDoSaTo").join(f), "").unwrap();
        }
        assert_eq!(ds2.missing_files(t.path()), None);

        // Gamebryo masters are looked up in the plugin folder.
        let sky = super::find("Skyrim").unwrap();
        assert!(sky.missing_files(t.path()).unwrap().contains("Skyrim.esm"));
        std::fs::create_dir_all(t.path().join("Data")).unwrap();
        std::fs::write(t.path().join("Data/Skyrim.esm"), "").unwrap();
        std::fs::write(t.path().join("Data/Update.esm"), "").unwrap();
        assert_eq!(sky.missing_files(t.path()), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn executable_falls_back_to_native_binary() {
        let sdv = super::find("stardewvalley").unwrap();
        let dir = tempfile::tempdir().unwrap();
        // No native binary: keep the .exe path (error names what the definition asked for).
        assert_eq!(
            sdv.executable(dir.path(), None).unwrap(),
            dir.path().join("Stardew Valley.exe")
        );
        std::fs::write(dir.path().join("Stardew Valley"), "").unwrap();
        assert_eq!(
            sdv.executable(dir.path(), None).unwrap(),
            dir.path().join("Stardew Valley")
        );
        // Custom command (SMAPI) wins, with the same fallback.
        std::fs::write(dir.path().join("StardewModdingAPI"), "").unwrap();
        let smapi = sdv.executable(dir.path(), Some("StardewModdingAPI.exe"));
        assert_eq!(smapi.unwrap(), dir.path().join("StardewModdingAPI"));
    }
}
