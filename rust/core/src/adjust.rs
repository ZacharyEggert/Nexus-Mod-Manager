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
    BaldursGate3,
    Bannerlord,
    BreakingWheel,
    Cyberpunk2077,
    DarkSouls2,
    DragonAge,
    DragonsDogma,
    Fallout4,
    HogwartsLegacy,
    MonsterHunterWorld,
    NoMansSky,
    OblivionRemastered,
    Sims4,
    StardewValley,
    Starfield,
    StateOfDecay,
    Subnautica,
    Witcher2,
    Witcher3,
    XRebirth,
}

/// What a path rule may need to know about the whole mod (C# rules read the `IMod`).
#[derive(Debug, Default, Clone)]
pub struct Mod<'a> {
    pub name: &'a str,
    /// Archive file name (`Mod.dazip`).
    pub archive: &'a str,
    /// Every file the mod installs, before adjustment, in archive order.
    pub files: &'a [String],
    pub stop_folders: &'a [String],
    /// `content.xml`'s root `name` when the mod's files go under it (State of Decay, X Rebirth).
    pub content_name: Option<String>,
}

impl Mod<'_> {
    fn archive_stem(&self) -> &str {
        let n = file_name(self.archive);
        n.rfind('.').map_or(n, |i| &n[..i])
    }

    fn archive_is(&self, e: &str) -> bool {
        ext(self.archive) == e
    }
}

/// Per-mod checks before any file is adjusted, as the C# rules threw mid-install. Returns the
/// `content.xml` folder name to put in `Mod::content_name`. `read` gets a file's bytes by path.
pub fn prepare(
    profile: Profile,
    files: &[String],
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> Result<Option<String>, String> {
    if !matches!(profile, Profile::StateOfDecay | Profile::XRebirth) {
        return Ok(None);
    }
    const MULTI: &str = "This package contains multiple mods, which is not supported.";
    let contents: Vec<&String> = files.iter().filter(|f| f.contains("content.xml")).collect();
    let name = match (profile, contents.as_slice()) {
        (Profile::StateOfDecay, []) => None,
        (Profile::XRebirth, []) => {
            return Err("Mod package does not include a content.xml file.".into())
        }
        (_, [one]) => {
            let nest = one.split('/').count();
            if nest == 1 || nest > 2 {
                Some(content_name(&read(one).unwrap_or_default())?)
            } else {
                None
            }
        }
        _ => return Err(MULTI.into()),
    };
    if profile == Profile::StateOfDecay && name.is_none() {
        let bad = files.iter().find(|f| {
            let l = f.to_lowercase();
            sod_keep(f)
                && f.contains('/')
                && !["game", "engine", "user"].iter().any(|r| l.starts_with(r))
        });
        if bad.is_some() {
            return Err("This package does not meet the layout standards for State of Decay mods, which is not supported.".into());
        }
    }
    Ok(name)
}

fn content_name(xml: &[u8]) -> Result<String, String> {
    let bad = || "The attribute 'name' is set improperly in the file 'content.xml'.".to_string();
    let text = String::from_utf8_lossy(xml);
    let doc = roxmltree::Document::parse(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("content.xml: {e}"))?;
    doc.root_element()
        .attribute("name")
        .filter(|n| !n.is_empty())
        .map(String::from)
        .ok_or_else(bad)
}

/// State of Decay skips docs and package metadata (C# `validPathForModifications`).
fn sod_keep(p: &str) -> bool {
    let l = p.to_lowercase();
    !(p.contains("content.xml")
        || p.contains("components.xml")
        || ["readme", "read me", ".url", "jsgme"]
            .iter()
            .any(|s| l.contains(s)))
}

fn starts_ci(p: &str, prefix: &str) -> bool {
    p.get(..prefix.len())
        .is_some_and(|h| h.eq_ignore_ascii_case(prefix))
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

pub fn adjust(profile: Profile, path: &str, m: &Mod) -> Option<Dest> {
    let p = path.replace('\\', "/");
    let p = p.trim_start_matches('/');
    let mod_name = m.name;
    let install = |s: String| Some(Dest::Install(s));
    let named = |s: &str| match &m.content_name {
        Some(n) => format!("{n}/{s}"),
        None => s.into(),
    };
    match profile {
        Profile::BaldursGate3 => baldurs_gate3(p),
        Profile::Bannerlord => install(if p.starts_with("Modules") {
            p.into()
        } else {
            format!("Modules/{p}")
        }),
        Profile::BreakingWheel => install(if starts_ci(p, "Content") {
            format!("Main Game/Ellie_Ball_Project/{p}")
        } else {
            format!("Main Game/Ellie_Ball_Project/Content/{p}")
        }),
        Profile::DarkSouls2 => dark_souls2(p).map(Dest::Install),
        Profile::DragonAge => dragon_age(p, m),
        // C# `Contains`: a stop folder name anywhere in the path, case-sensitive.
        Profile::DragonsDogma => m
            .stop_folders
            .iter()
            .any(|s| p.contains(s.as_str()))
            .then(|| Dest::Install(p.into())),
        Profile::HogwartsLegacy => {
            let p = if p.starts_with("mods") {
                format!("~{p}")
            } else {
                p.into()
            };
            install(if p.starts_with("~mods") {
                p
            } else {
                format!("~mods/{p}")
            })
        }
        Profile::MonsterHunterWorld => {
            let loader = ["dtdata.dll", "loader.dll", "loader-config.json"].contains(&p);
            install(if starts_ci(p, "nativePC") || loader {
                p.into()
            } else {
                format!("nativePC/{p}")
            })
        }
        Profile::StateOfDecay => match &m.content_name {
            Some(_) => install(named(p)),
            None => sod_keep(p).then(|| Dest::Install(p.into())),
        },
        Profile::XRebirth => install(named(p)),
        Profile::Witcher2 => Some(witcher2(p, m)),
        Profile::Witcher3 => witcher3(p, mod_name).map(Dest::Install),
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

/// `info.json` feeds modsettings.lsx instead of deploying; `bin/` and `Data/` trees go to the game folder.
fn baldurs_gate3(p: &str) -> Option<Dest> {
    if file_name(p).eq_ignore_ascii_case("info.json") {
        return None;
    }
    let l = p.to_lowercase();
    let game = ["bin/", "data/"]
        .iter()
        .any(|d| l.starts_with(d) || l.contains(&format!("/{d}")));
    Some(if game {
        Dest::GameRoot(p.into())
    } else {
        Dest::Install(p.into())
    })
}

/// GeDoSaTo's `textures/DarkSoulsII/override`; C# `Substring`s, case-sensitive.
fn dark_souls2(p: &str) -> Option<String> {
    let out = if p.starts_with("DarkSoulsII") {
        p.get(12..).unwrap_or("").into()
    } else if p.starts_with("textures") {
        p.get(21..).unwrap_or("").into()
    } else if !p.contains("override") {
        format!("override/{p}")
    } else {
        p.into()
    };
    (!out.is_empty()).then_some(out)
}

/// `.dazip` content goes to Documents/BioWare/Dragon Age (secondary root), `contents/` stripped;
/// its Manifest.xml is read for AddIns.xml, not deployed. Loose root files get a mod-named folder.
fn dragon_age(p: &str, m: &Mod) -> Option<Dest> {
    let dazip = m.archive_is(".dazip");
    let mut s = String::new();
    if dazip {
        if file_name(p).eq_ignore_ascii_case("manifest.xml") {
            return None;
        }
        if starts_ci(p, "contents") {
            s = p.get(9..).unwrap_or("").into();
        }
    }
    if s.is_empty() {
        s = if depth(p) == 0 {
            m.archive_stem().into()
        } else {
            p.into()
        };
    }
    let out = if depth(p) == 0 { format!("{s}/{p}") } else { s };
    Some(if dazip {
        Dest::GameRoot(out)
    } else {
        Dest::Install(out)
    })
}

/// Mods with a `cook.hash` are user content (secondary root, Documents/Witcher 2/UserContent):
/// root files go in a folder named after the first file's folder (or the archive), spaces dropped.
fn witcher2(p: &str, m: &Mod) -> Dest {
    if !m.files.iter().any(|f| f.contains("cook.hash")) {
        return Dest::Install(p.into());
    }
    if depth(p) > 0 {
        return Dest::GameRoot(p.into());
    }
    let dir = m
        .files
        .first()
        .and_then(|f| f.rsplit_once('/'))
        .map_or(m.archive_stem(), |(d, _)| d)
        .replace(' ', "");
    Dest::GameRoot(format!("{dir}/{p}"))
}

/// `content/` trees become `Mods/mod<Name>/content`, `mod*` / `DLC*` folders go under `Mods/` /
/// `DLC/`, checked as-is then without the outer folder. Anything else isn't installed.
/// C# put loose `content/` in `<game>/mod<Name>` (outside `Mods`, where the game never looks).
fn witcher3(p: &str, mod_name: &str) -> Option<String> {
    let clean: String = mod_name
        .chars()
        .filter(|c| c.is_alphanumeric() || "_.-".contains(*c))
        .collect();
    let candidates = [Some(p), p.split_once('/').map(|(_, rest)| rest)];
    candidates.into_iter().flatten().find_map(|c| {
        if starts_ci(c, "content") {
            Some(format!("Mods/mod{clean}/{c}"))
        } else if c.starts_with("mod") {
            Some(format!("Mods/{c}"))
        } else if c.starts_with("DLC") {
            Some(format!("DLC/{c}"))
        } else {
            None
        }
    })
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
    use super::{adjust, prepare, Dest::*, Mod, Profile::*};

    #[test]
    fn profiles() {
        let a = |pr, p: &str| {
            adjust(
                pr,
                p,
                &Mod {
                    name: "My Mod!",
                    files: &[p.to_string()],
                    ..Mod::default()
                },
            )
        };
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

        assert_eq!(a(BaldursGate3, "Foo/info.json"), Option::None);
        assert_eq!(a(BaldursGate3, "Foo.pak"), i("Foo.pak"));
        assert_eq!(a(BaldursGate3, "bin/x.dll"), g("bin/x.dll"));
        assert_eq!(a(BaldursGate3, "Wrap/Data/x.lsx"), g("Wrap/Data/x.lsx"));

        assert_eq!(a(Bannerlord, "Modules/X/a.xml"), i("Modules/X/a.xml"));
        assert_eq!(a(Bannerlord, "X/a.xml"), i("Modules/X/a.xml"));

        assert_eq!(
            a(BreakingWheel, "content/a.pak"),
            i("Main Game/Ellie_Ball_Project/content/a.pak")
        );
        assert_eq!(
            a(BreakingWheel, "a.pak"),
            i("Main Game/Ellie_Ball_Project/Content/a.pak")
        );

        assert_eq!(
            a(DarkSouls2, "DarkSoulsII/override/a.dds"),
            i("override/a.dds")
        );
        assert_eq!(
            a(DarkSouls2, "textures/DarkSoulsII/override/a.dds"),
            i("override/a.dds")
        );
        assert_eq!(a(DarkSouls2, "a.dds"), i("override/a.dds"));

        assert_eq!(a(HogwartsLegacy, "mods/a.pak"), i("~mods/a.pak"));
        assert_eq!(a(HogwartsLegacy, "a.pak"), i("~mods/a.pak"));
        assert_eq!(a(HogwartsLegacy, "~mods/a.pak"), i("~mods/a.pak"));

        assert_eq!(a(MonsterHunterWorld, "loader.dll"), i("loader.dll"));
        assert_eq!(a(MonsterHunterWorld, "NativePC/a"), i("NativePC/a"));
        assert_eq!(a(MonsterHunterWorld, "pl/a"), i("nativePC/pl/a"));

        assert_eq!(
            a(Witcher3, "content/a.bundle"),
            i("Mods/modMyMod/content/a.bundle")
        );
        assert_eq!(a(Witcher3, "Wrap/modX/content/a"), i("Mods/modX/content/a"));
        assert_eq!(a(Witcher3, "DLC1/a"), i("DLC/DLC1/a"));
        assert_eq!(a(Witcher3, "readme.txt"), Option::None);
    }

    #[test]
    fn whole_mod_rules() {
        let files = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let i = |s: &str| Some(Install(s.into()));
        let g = |s: &str| Some(GameRoot(s.into()));

        // Dragon's Dogma: only paths naming a stop folder install.
        let stop = files(&["rom", "sound"]);
        let dd = Mod {
            stop_folders: &stop,
            ..Mod::default()
        };
        assert_eq!(adjust(DragonsDogma, "rom/a.arc", &dd), i("rom/a.arc"));
        assert_eq!(adjust(DragonsDogma, "readme.txt", &dd), Option::None);

        // Dragon Age: .dazip contents go to the secondary root, the manifest is held back.
        let dazip = Mod {
            archive: "Cool.dazip",
            ..Mod::default()
        };
        assert_eq!(adjust(DragonAge, "Manifest.xml", &dazip), Option::None);
        assert_eq!(
            adjust(DragonAge, "Contents/addins/x/a.erf", &dazip),
            g("addins/x/a.erf")
        );
        let plain = Mod {
            archive: "Cool.zip",
            ..Mod::default()
        };
        assert_eq!(adjust(DragonAge, "a.erf", &plain), i("Cool/a.erf"));
        assert_eq!(adjust(DragonAge, "sub/a.erf", &plain), i("sub/a.erf"));

        // Witcher 2: a cook.hash makes it user content, root files under the first file's folder.
        let w2 = files(&["My Mod/cook.hash", "My Mod/a.dzip", "b.dzip"]);
        let user = Mod {
            files: &w2,
            archive: "x.7z",
            ..Mod::default()
        };
        assert_eq!(adjust(Witcher2, "My Mod/a.dzip", &user), g("My Mod/a.dzip"));
        assert_eq!(adjust(Witcher2, "b.dzip", &user), g("MyMod/b.dzip"));
        let cooked = files(&["a.dzip"]);
        let base = Mod {
            files: &cooked,
            ..Mod::default()
        };
        assert_eq!(adjust(Witcher2, "a.dzip", &base), i("a.dzip"));

        // content.xml: a root one names the mod folder; one folder deep stays as is.
        let read = |_: &str| Some(br#"<content name="Cool Mod" save="true"/>"#.to_vec());
        let root = files(&["content.xml", "ext/a.cat"]);
        assert_eq!(prepare(XRebirth, &root, &read), Ok(Some("Cool Mod".into())));
        let nested = files(&["Cool/content.xml", "Cool/a.cat"]);
        assert_eq!(prepare(XRebirth, &nested, &read), Ok(Option::None));
        assert!(prepare(XRebirth, &files(&["a.cat"]), &read).is_err());
        let two = files(&["a/content.xml", "b/content.xml"]);
        assert!(prepare(StateOfDecay, &two, &read)
            .unwrap_err()
            .contains("multiple mods"));
        let named = Mod {
            content_name: Some("Cool Mod".into()),
            ..Mod::default()
        };
        assert_eq!(
            adjust(XRebirth, "ext/a.cat", &named),
            i("Cool Mod/ext/a.cat")
        );
        assert_eq!(
            adjust(StateOfDecay, "readme.txt", &named),
            i("Cool Mod/readme.txt")
        );

        // State of Decay without content.xml: docs skipped, other folders must be game/engine/user.
        let sod = files(&["game/a.pak", "ReadMe.txt"]);
        assert_eq!(prepare(StateOfDecay, &sod, &read), Ok(Option::None));
        assert_eq!(
            adjust(StateOfDecay, "ReadMe.txt", &Mod::default()),
            Option::None
        );
        assert_eq!(
            adjust(StateOfDecay, "game/a.pak", &Mod::default()),
            i("game/a.pak")
        );
        assert!(prepare(StateOfDecay, &files(&["mods/a.pak"]), &read).is_err());
        let bad = |_: &str| Some(b"<content/>".to_vec());
        assert!(prepare(XRebirth, &root, &bad)
            .unwrap_err()
            .contains("'name'"));
    }
}
