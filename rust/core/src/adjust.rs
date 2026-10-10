//! Per-game path rewrites (`modInstall.pathAdjustmentProfile`), ported from
//! `NexusClient/Games/DataDriven/DataDrivenGameMode.cs` (`*PathAdjuster`).
//!
//! Input: an archive path relative to the mod's base folder, `/`-separated.
//! Output: where it deploys, or `None` to skip the file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    #[default]
    None,
    Cyberpunk2077,
    Fallout4,
    NoMansSky,
    OblivionRemastered,
    Sims4,
    StardewValley,
    Starfield,
    Subnautica,
}

#[derive(Debug, PartialEq)]
pub enum Dest {
    /// Relative to the game's install root (Data, managed path, or game folder).
    Install(String),
    /// Relative to the game folder, outside the install root.
    GameRoot(String),
}

/// Same path or a child of `root`, ignoring case.
fn under(path: &str, root: &str) -> bool {
    let (p, r) = (path.to_ascii_lowercase(), root.to_ascii_lowercase());
    p == r || p.starts_with(&format!("{r}/"))
}

/// `path` with `root/` removed; `path` must be `under` it.
fn after<'a>(path: &'a str, root: &str) -> &'a str {
    path.get(root.len() + 1..).unwrap_or("")
}

fn first(path: &str) -> &str {
    path.split('/').next().unwrap_or(path)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn ext(path: &str) -> String {
    let name = file_name(path);
    name.rfind('.')
        .map(|i| name[i..].to_ascii_lowercase())
        .unwrap_or_default()
}

fn depth(path: &str) -> usize {
    path.matches('/').count()
}

pub fn adjust(profile: Profile, path: &str, mod_name: &str) -> Option<Dest> {
    let p = path.replace('\\', "/");
    let p = p.trim_start_matches('/');
    let install = |s: String| Some(Dest::Install(s));
    match profile {
        Profile::None => install(p.into()),
        Profile::Fallout4 => install(strip_data(&fallout4(p))),
        Profile::Starfield => install(strip_data(&starfield(p))),
        Profile::OblivionRemastered => Some(oblivion_remastered(p)),
        Profile::StardewValley => install(if under(p, "Mods") {
            p.into()
        } else {
            format!("Mods/{p}")
        }),
        Profile::Subnautica => install(subnautica(p)),
        Profile::Sims4 => sims4(p).map(Dest::Install),
        Profile::NoMansSky => no_mans_sky(p, mod_name).map(Dest::Install),
        Profile::Cyberpunk2077 => install(cyberpunk(p)),
    }
}

/// Port of `MakePluginDirectoryRelative` for the `Data` plugin dir.
fn strip_data(p: &str) -> String {
    if under(p, "Data") {
        after(p, "Data").into()
    } else {
        p.into()
    }
}

/// A plugin one wrapper folder deep moves into Data.
fn fallout4(p: &str) -> String {
    let is_plugin = [".esm", ".esl", ".esp"].contains(&ext(p).as_str());
    if depth(p) == 1 && is_plugin && !first(p).eq_ignore_ascii_case("Data") {
        format!("Data/{}", file_name(p))
    } else {
        p.into()
    }
}

fn starfield(p: &str) -> String {
    if !under(p, "Data") && ext(p) == ".esm" && depth(p) <= 1 {
        format!("Data/{}", file_name(p))
    } else {
        p.into()
    }
}

const OR_DATA: &str = "OblivionRemastered/Content/Dev/ObvData/Data";
const OR_WIN64: &str = "OblivionRemastered/Binaries/Win64";
const OR_PAKS: &str = "OblivionRemastered/Content/Paks";

fn oblivion_remastered(p: &str) -> Dest {
    let game_root = |s: String| {
        if under(&s, OR_DATA) {
            Dest::Install(after(&s, OR_DATA).into())
        } else {
            Dest::GameRoot(s)
        }
    };
    let e = ext(p);
    if under(p, "OblivionRemastered") || under(p, "Engine") {
        game_root(p.into())
    } else if under(p, "Data") {
        Dest::Install(after(p, "Data").into())
    } else if under(p, "Binaries") || under(p, "Content") {
        game_root(format!("OblivionRemastered/{p}"))
    } else if under(p, "Paks") {
        game_root(
            format!("{OR_PAKS}/{}", after(p, "Paks"))
                .trim_end_matches('/')
                .into(),
        )
    } else if under(p, "~mods") || under(p, "LogicMods") {
        game_root(format!("{OR_PAKS}/{p}"))
    } else if under(p, "OBSE") {
        game_root(format!("{OR_WIN64}/{p}"))
    } else if [".pak", ".ucas", ".utoc"].contains(&e.as_str()) {
        game_root(format!("{OR_PAKS}/~mods/{}", file_name(p)))
    } else if [".asi", ".dll", ".exe"].contains(&e.as_str()) && depth(p) == 0 {
        game_root(format!("{OR_WIN64}/{p}"))
    } else {
        // C#'s data-folder / plugin-extension cases and its fallback all land in Data.
        Dest::Install(p.into())
    }
}

fn subnautica(p: &str) -> String {
    const BEPINEX_CHILDREN: &[&str] = &["cache", "config", "core", "patchers", "plugins"];
    const ROOT_FILES: &[&str] = &[
        ".doorstop_version",
        "doorstop_config.ini",
        "steam_appid.txt",
        "winhttp.dll",
    ];
    if ["BepInEx", "QMods", "Subnautica_Data", "SubnauticaZero_Data"]
        .iter()
        .any(|r| under(p, r))
    {
        p.into()
    } else if BEPINEX_CHILDREN
        .iter()
        .any(|c| first(p).eq_ignore_ascii_case(c))
        && depth(p) > 0
    {
        format!("BepInEx/{p}")
    } else if depth(p) == 0 && ROOT_FILES.iter().any(|f| p.eq_ignore_ascii_case(f)) {
        p.into()
    } else {
        format!("BepInEx/plugins/{p}")
    }
}

fn sims4(p: &str) -> Option<String> {
    let e = ext(p);
    if [".txt", ".png", ".jpg", ".pdf", ".doc", ".docx"].contains(&e.as_str()) {
        return None;
    }
    if ["Tray", "Mods", "saves"].iter().any(|r| under(p, r)) {
        return Some(p.into());
    }
    // Legacy wrapper cleanup: drop the outer folder of anything two or more deep.
    let p = if depth(p) >= 2 {
        p.split_once('/').unwrap().1
    } else {
        p
    };
    const TRAY: &[&str] = &[
        ".householdbinary",
        ".trayitem",
        ".sgi",
        ".hhi",
        ".blueprint",
        ".bpi",
        ".room",
        ".rmi",
    ];
    Some(if TRAY.contains(&e.as_str()) {
        format!("Tray/{p}")
    } else if e == ".save" {
        format!("saves/{p}")
    } else {
        format!("Mods/{p}")
    })
}

/// Stands in for the mod's load-order slot in NMS folder names (`{order}_Name` -> `0003_Name`).
/// The game loads `GAMEDATA/MODS` folders alphabetically, so deploy fills it in from load order.
pub const ORDER: &str = "{order}";

/// Fill in `ORDER` for a mod at load-order position `index`.
pub fn resolve(path: &str, index: usize) -> String {
    path.replace(ORDER, &format!("{index:04}"))
}

fn no_mans_sky(p: &str, mod_name: &str) -> Option<String> {
    const CONTENT: &[&str] = &[
        "AUDIO",
        "FONTS",
        "GLOBALS",
        "LANGUAGE",
        "MATERIALS",
        "METADATA",
        "MODELS",
        "PIPELINES",
        "SCENES",
        "SHADERS",
        "TEXTURES",
        "UI",
    ];
    let e = ext(p);
    if e == ".pak" {
        return None; // 5.50+ no longer loads legacy PSARC mods.
    }
    if under(p, "Binaries") {
        return Some(p.into());
    }
    if depth(p) == 0 && (e == ".dll" || e == ".exe") {
        return Some(format!("Binaries/{p}"));
    }
    let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    let is = |i: usize, s: &str| segs.get(i).is_some_and(|x| x.eq_ignore_ascii_case(s));
    let mut i = if is(0, "GAMEDATA") && is(1, "MODS") {
        2
    } else if is(0, "MODS") || is(0, "GAMEDATA") {
        1
    } else {
        0
    };
    let content = |i: usize| {
        segs.get(i)
            .is_some_and(|s| CONTENT.iter().any(|c| c.eq_ignore_ascii_case(s)))
    };
    if segs.len() >= i + 2 && !content(i) && content(i + 1) {
        i += 1; // the archive's own outer mod-name folder
    }
    let rest = if i < segs.len() {
        segs[i..].join("/")
    } else {
        file_name(p).into()
    };
    Some(format!(
        "GAMEDATA/MODS/{ORDER}_{}/{rest}",
        nms_folder(mod_name)
    ))
}

fn nms_folder(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._ -".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    let safe = safe.trim_matches(['.', ' ']);
    let safe = if safe.is_empty() { "NMMMod" } else { safe };
    // The game caps mod-folder names; char-boundary safe since `safe` is ASCII.
    safe[..safe.len().min(96)]
        .trim_end_matches(['.', ' '])
        .into()
}

/// Port of `CyberpunkPathAdjuster` for game 1.2+ (`patch` folders became `mod`).
/// Matches whole path segments where C# matched raw string prefixes, and doesn't double
/// the `archive/pc/mod` prefix the way C#'s fallthrough did.
fn cyberpunk(p: &str) -> String {
    if under(p, "bin") {
        return p.into();
    }
    if under(p, "x64") {
        return format!("bin/{p}");
    }
    let p: String = p
        .split('/')
        .map(|s| {
            if s.eq_ignore_ascii_case("patch") {
                "mod"
            } else {
                s
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    if ["archive", "red4ext", "mods", "engine", "r6"]
        .iter()
        .any(|r| under(&p, r))
    {
        p
    } else if under(&p, "pc") {
        format!("archive/{p}")
    } else if under(&p, "mod") {
        format!("archive/pc/{p}")
    } else {
        format!("archive/pc/mod/{p}")
    }
}

#[cfg(test)]
mod tests {
    use super::{adjust, Dest::*, Profile::*};

    #[test]
    fn profiles() {
        let a = |pr, p| adjust(pr, p, "My Mod!");
        let i = |s: &str| Some(Install(s.into()));
        let g = |s: &str| Some(GameRoot(s.into()));

        assert_eq!(a(None, "textures/a.dds"), i("textures/a.dds"));

        assert_eq!(a(Fallout4, "Wrap/A.esp"), i("A.esp"));
        assert_eq!(a(Fallout4, "Data/A.esp"), i("A.esp"));
        assert_eq!(a(Fallout4, "textures/a.dds"), i("textures/a.dds"));

        assert_eq!(a(Starfield, "Wrap/A.esm"), i("A.esm"));
        assert_eq!(a(Starfield, "a/b/A.esm"), i("a/b/A.esm"));

        assert_eq!(a(OblivionRemastered, "Data/A.esp"), i("A.esp"));
        assert_eq!(a(OblivionRemastered, "textures/a.dds"), i("textures/a.dds"));
        assert_eq!(
            a(OblivionRemastered, "obse64_loader.exe"),
            g("OblivionRemastered/Binaries/Win64/obse64_loader.exe")
        );
        assert_eq!(
            a(OblivionRemastered, "OBSE/Plugins/x.dll"),
            g("OblivionRemastered/Binaries/Win64/OBSE/Plugins/x.dll")
        );
        assert_eq!(
            a(OblivionRemastered, "sub/x.pak"),
            g("OblivionRemastered/Content/Paks/~mods/x.pak")
        );
        assert_eq!(
            a(
                OblivionRemastered,
                "OblivionRemastered/Content/Dev/ObvData/Data/A.esp"
            ),
            i("A.esp")
        );

        assert_eq!(
            a(StardewValley, "Foo/manifest.json"),
            i("Mods/Foo/manifest.json")
        );
        assert_eq!(a(StardewValley, "Mods/Foo/a.dll"), i("Mods/Foo/a.dll"));

        assert_eq!(
            a(Subnautica, "plugins/X/x.dll"),
            i("BepInEx/plugins/X/x.dll")
        );
        assert_eq!(a(Subnautica, "winhttp.dll"), i("winhttp.dll"));
        assert_eq!(a(Subnautica, "X/x.dll"), i("BepInEx/plugins/X/x.dll"));
        assert_eq!(a(Subnautica, "QMods/X/x.dll"), i("QMods/X/x.dll"));

        assert_eq!(a(Sims4, "readme.txt"), Option::None);
        assert_eq!(a(Sims4, "Wrap/Sub/x.package"), i("Mods/Sub/x.package"));
        assert_eq!(a(Sims4, "x.trayitem"), i("Tray/x.trayitem"));
        assert_eq!(a(Sims4, "Mods/x.package"), i("Mods/x.package"));

        assert_eq!(a(NoMansSky, "old.pak"), Option::None);
        assert_eq!(a(NoMansSky, "inject.dll"), i("Binaries/inject.dll"));
        assert_eq!(
            a(NoMansSky, "MyMod/MODELS/a.mbin"),
            i("GAMEDATA/MODS/{order}_My Mod_/MODELS/a.mbin")
        );
        assert_eq!(
            a(NoMansSky, "GAMEDATA/MODS/X/TEXTURES/a.dds"),
            i("GAMEDATA/MODS/{order}_My Mod_/TEXTURES/a.dds")
        );

        assert_eq!(
            a(Cyberpunk2077, "x64/plugins/a.dll"),
            i("bin/x64/plugins/a.dll")
        );
        assert_eq!(a(Cyberpunk2077, "a.archive"), i("archive/pc/mod/a.archive"));
        assert_eq!(
            a(Cyberpunk2077, "patch/a.archive"),
            i("archive/pc/mod/a.archive")
        );
        assert_eq!(
            a(Cyberpunk2077, "pc/mod/a.archive"),
            i("archive/pc/mod/a.archive")
        );
        assert_eq!(
            a(Cyberpunk2077, "r6/scripts/a.reds"),
            i("r6/scripts/a.reds")
        );
        assert_eq!(
            a(Cyberpunk2077, "dispatcher.archive"),
            i("archive/pc/mod/dispatcher.archive")
        );
    }
}
