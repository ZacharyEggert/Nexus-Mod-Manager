//! What Launch can start (C# `DataDrivenGameLauncher` + supported tools) and how to run it on this
//! OS: a Windows `.exe` off Windows goes through Steam / Proton, CrossOver, Whisky or Wine.

use crate::game::GameDef;
use crate::tools::Tool;
use crate::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const PLAIN: &str = "plain";
pub const SCRIPT_EXTENDER: &str = "script_extender";
pub const CUSTOM: &str = "custom";
pub const TOOL: &str = "tool:";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LaunchOption {
    /// `plain`, `script_extender`, `custom` or `tool:<id>`.
    pub id: String,
    pub label: String,
    /// What it runs; None for a tool not found yet (the UI offers to set its folder).
    pub exe: Option<PathBuf>,
}

/// The user's per-game launch settings.
#[derive(Debug, Default, Clone)]
pub struct Settings {
    pub custom: Option<String>,
    pub custom_args: Option<String>,
    /// Tool settings key -> folder.
    pub tools: BTreeMap<String, PathBuf>,
}

/// Every launch option, plus the id plain Launch uses (C# `LaunchDefault`: custom command, else
/// script extender, else the game).
pub fn options(
    game: &GameDef,
    game_path: &Path,
    s: &Settings,
    registry: &crate::discover::Registry,
) -> (Vec<LaunchOption>, String) {
    let name = &game.name;
    let label = game
        .launcher
        .as_ref()
        .and_then(|l| l.plain_command_text.clone())
        .unwrap_or_else(|| format!("Launch {name}"));
    let mut out = vec![LaunchOption {
        id: PLAIN.into(),
        label,
        exe: game.executable(game_path, None),
    }];
    let se = game.script_extender(game_path);
    if let Some((se_name, exe)) = &se {
        out.push(LaunchOption {
            id: SCRIPT_EXTENDER.into(),
            label: format!("Launch {se_name}"),
            exe: Some(exe.clone()),
        });
    }
    let custom = s
        .custom
        .as_deref()
        .filter(|c| !c.trim().is_empty() && game.allows_custom_command());
    if let Some(c) = custom {
        out.push(LaunchOption {
            id: CUSTOM.into(),
            label: format!("Launch Custom {name}"),
            exe: game.executable(game_path, Some(c)),
        });
    }
    for t in crate::tools::of(game) {
        let exe = t.find(
            game,
            game_path,
            s.tools.get(t.settings_key()).map(PathBuf::as_path),
            registry,
        );
        out.push(LaunchOption {
            id: format!("{TOOL}{}", t.id),
            label: format!("Launch {}", t.name),
            exe,
        });
    }
    let default = if custom.is_some() {
        CUSTOM
    } else if se.is_some() {
        SCRIPT_EXTENDER
    } else {
        PLAIN
    };
    (out, default.into())
}

/// The tool behind a `tool:<id>` option.
pub fn tool(game: &GameDef, id: &str) -> Option<Tool> {
    let id = id.strip_prefix(TOOL)?;
    crate::tools::of(game).into_iter().find(|t| t.id == id)
}

/// A process to start.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// Split a custom-arguments string like a Windows command line: spaces separate, quotes group.
pub fn split_args(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted, mut any) = (vec![], String::new(), false, false);
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// How to start `exe` with `args` for `game` at `game_path`. `plain` = the game's own launch,
/// which a Steam game off Windows starts through Steam (DRM, overlay, the user's Proton choice).
pub fn plan(
    game: &GameDef,
    game_path: &Path,
    exe: &Path,
    args: Vec<String>,
    plain: bool,
    steam_roots: &[PathBuf],
) -> Result<Plan> {
    let cwd = exe
        .parent()
        .filter(|p| p.is_dir())
        .unwrap_or(game_path)
        .to_path_buf();
    let direct = |program: PathBuf, args: Vec<String>, env: Vec<(String, String)>| Plan {
        program,
        args,
        env,
        cwd: cwd.clone(),
    };
    if exe.extension().is_some_and(|e| e == "app") {
        let mut a = vec![exe.to_string_lossy().into_owned()];
        if !args.is_empty() {
            a.push("--args".into());
            a.extend(args);
        }
        return Ok(direct("open".into(), a, vec![]));
    }
    let windows_exe = exe
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    if cfg!(windows) || !windows_exe {
        return Ok(direct(exe.into(), args, vec![]));
    }
    let exe_arg = exe.to_string_lossy().into_owned();
    let with_exe = |mut pre: Vec<String>| {
        pre.push(exe_arg.clone());
        pre.extend(args.clone());
        pre
    };

    // Steam Play: the game's library holds compatdata/<appid>.
    let steamapps = game_path.ancestors().find(|a| {
        a.file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("steamapps"))
    });
    let steam = steamapps.and_then(|sa| {
        game.discovery
            .stores
            .iter()
            .filter(|s| s.store.eq_ignore_ascii_case("steam"))
            .filter_map(|s| s.id.clone())
            .find(|id| sa.join("compatdata").join(id).join("pfx").is_dir())
            .map(|id| (sa.join("compatdata").join(&id), id))
    });
    if let Some((compat, id)) = steam {
        if plain {
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            return Ok(direct(
                opener.into(),
                vec![format!("steam://rungameid/{id}")],
                vec![],
            ));
        }
        let (proton, root) = proton(steam_roots, &id).ok_or(
            "No Proton found: install one in Steam (or GE-Proton in compatibilitytools.d) to run this outside Steam",
        )?;
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let env = vec![
            ("STEAM_COMPAT_DATA_PATH".into(), s(&compat)),
            ("STEAM_COMPAT_CLIENT_INSTALL_PATH".into(), s(&root)),
            ("SteamAppId".into(), id.clone()),
            ("SteamGameId".into(), id),
        ];
        return Ok(direct(proton, with_exe(vec!["run".into()]), env));
    }

    // A Wine prefix: the game folder's drive_c, else the user folder Heroic / Lutris gave.
    let prefix = game_path
        .ancestors()
        .find(|a| {
            a.file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("drive_c"))
        })
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(|| {
            // <prefix>/drive_c/users/<name>
            Some(
                game.user_dir
                    .as_deref()?
                    .parent()?
                    .parent()?
                    .parent()?
                    .to_path_buf(),
            )
        });
    let Some(prefix) = prefix else {
        return Err(format!(
            "{} is a Windows program, and no Proton prefix or Wine bottle was found for this game",
            exe.display()
        )
        .into());
    };
    let wineprefix = vec![("WINEPREFIX".into(), prefix.to_string_lossy().into_owned())];
    let parent_name = |p: &Path| {
        p.parent()
            .and_then(|x| x.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    };
    let in_bottles_of = |app: &str| {
        parent_name(&prefix).as_deref() == Some("Bottles")
            && prefix.parent().and_then(parent_name_of).as_deref() == Some(app)
    };
    if in_bottles_of("CrossOver") {
        let bottle = prefix.file_name().unwrap().to_string_lossy().into_owned();
        let wine =
            PathBuf::from("/Applications/CrossOver.app/Contents/SharedSupport/CrossOver/bin/wine");
        return Ok(direct(
            wine,
            with_exe(vec!["--bottle".into(), bottle]),
            vec![],
        ));
    }
    if in_bottles_of("com.isaacmarovitz.Whisky") {
        let wine = dirs::home_dir()
            .unwrap_or_default()
            .join("Library/Application Support/com.isaacmarovitz.Whisky/Libraries/Wine/bin/wine64");
        return Ok(direct(wine, with_exe(vec![]), wineprefix));
    }
    Ok(direct("wine".into(), with_exe(vec![]), wineprefix))
}

fn parent_name_of(p: &Path) -> Option<String> {
    p.parent()?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// The Proton to run app `id` with, and the Steam root it belongs to: the tool Steam maps the
/// app (or all apps, `0`) to in `config/config.vdf`, else the newest official `Proton N`, else
/// Experimental, else any custom one in `compatibilitytools.d`.
pub fn proton(steam_roots: &[PathBuf], id: &str) -> Option<(PathBuf, PathBuf)> {
    let env = crate::discover::Env {
        steam_roots: vec![],
        registry: Box::new(|_, _| None),
        epic_manifests: None,
        xbox_roots: vec![],
        heroic: vec![],
        legendary: vec![],
        lutris: vec![],
        wine_prefixes: vec![],
        prefix: None,
    };
    for root in steam_roots {
        // (dir name, proton script)
        let mut found: Vec<(String, PathBuf)> = vec![];
        let mut scan = |dir: PathBuf, official: bool| {
            for e in std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
            {
                let name = e.file_name().to_string_lossy().into_owned();
                let script = e.path().join("proton");
                if script.is_file() && (!official || name.starts_with("Proton")) {
                    found.push((name, script));
                }
            }
        };
        scan(root.join("compatibilitytools.d"), false);
        for lib in crate::discover::libraries(root, &env) {
            scan(lib.join("steamapps/common"), true);
        }
        if found.is_empty() {
            continue;
        }
        let mapped = mapping(root, id).or_else(|| mapping(root, "0"));
        let version = |n: &str| -> Option<Vec<u32>> {
            let v = n.strip_prefix("Proton ")?;
            v.split('.').map(|x| x.parse().ok()).collect()
        };
        let pick = mapped
            .and_then(|m| {
                found.iter().find(|(n, _)| {
                    n == &m
                        || (m == "proton_experimental" && n == "Proton - Experimental")
                        || m.strip_prefix("proton_").is_some_and(|v| {
                            n.strip_prefix("Proton ")
                                .is_some_and(|nv| nv.split('.').next() == Some(v))
                        })
                })
            })
            .or_else(|| {
                found
                    .iter()
                    .filter(|(n, _)| version(n).is_some())
                    .max_by_key(|(n, _)| version(n))
            })
            .or_else(|| found.iter().find(|(n, _)| n == "Proton - Experimental"))
            .or(found.first());
        if let Some((_, p)) = pick {
            return Some((p.clone(), root.clone()));
        }
    }
    None
}

/// `CompatToolMapping` -> `"<id>"` -> `"name"` in Steam's config.vdf.
// ponytail: line scan of the VDF block, not a parser; fine for Steam's own formatting.
fn mapping(root: &Path, id: &str) -> Option<String> {
    let text = std::fs::read_to_string(root.join("config/config.vdf")).ok()?;
    let mut lines = text.lines().map(str::trim);
    lines.find(|l| {
        l.trim_matches('"')
            .eq_ignore_ascii_case("CompatToolMapping")
    })?;
    let mut depth = 0;
    let mut in_app = false;
    for l in lines {
        match l {
            "{" => depth += 1,
            "}" => {
                depth -= 1;
                if depth == 0 {
                    return None;
                }
                if depth == 1 {
                    in_app = false;
                }
            }
            _ if depth == 1 => in_app = l.trim_matches('"') == id,
            _ if depth == 2 && in_app => {
                let parts: Vec<&str> = l.split('"').collect();
                if parts.len() >= 4 && parts[1] == "name" && !parts[3].is_empty() {
                    return Some(parts[3].into());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "").unwrap();
    }

    #[test]
    fn options_and_default_order() {
        let t = tempfile::tempdir().unwrap();
        let sse = crate::game::find("SkyrimSE").unwrap();
        let none: crate::discover::Registry = Box::new(|_, _| None);
        let mut s = Settings::default();
        let ids = |s: &Settings| {
            let (o, d) = options(&sse, t.path(), s, &none);
            (o.into_iter().map(|o| o.id).collect::<Vec<_>>(), d)
        };
        let (o, d) = ids(&s);
        assert_eq!((o[0].as_str(), d.as_str()), (PLAIN, PLAIN));
        assert!(o.contains(&"tool:LOOT".to_string()) && !o.contains(&SCRIPT_EXTENDER.to_string()));
        // An installed script extender becomes the default; a custom command beats it.
        touch(&t.path().join("skse64_loader.exe"));
        let (o, d) = ids(&s);
        assert_eq!(
            (o[1].as_str(), d.as_str()),
            (SCRIPT_EXTENDER, SCRIPT_EXTENDER)
        );
        let (opts, _) = options(&sse, t.path(), &s, &none);
        assert_eq!(opts[1].label, "Launch SKSE");
        s.custom = Some("Custom.exe".into());
        let (o, d) = ids(&s);
        assert_eq!((o[2].as_str(), d.as_str()), (CUSTOM, CUSTOM));
        // A tool's configured folder is used.
        touch(&t.path().join("xedit/SSEEdit.exe"));
        s.tools.insert("SSEEdit".into(), t.path().join("xedit"));
        let (opts, _) = options(&sse, t.path(), &s, &none);
        let qac = opts.iter().find(|o| o.id == "tool:SSEEdit").unwrap();
        assert_eq!(
            qac.exe.as_deref(),
            Some(t.path().join("xedit/SSEEdit.exe").as_path())
        );
        assert_eq!(split_args(r#"-a "b c" "" d"#), ["-a", "b c", "", "d"]);
    }

    #[cfg(not(windows))]
    #[test]
    fn runners() {
        let t = tempfile::tempdir().unwrap();
        let sse = crate::game::find("SkyrimSE").unwrap();
        // Proton: compatdata in the library, Proton from config.vdf's mapping.
        let root = t.path().join("Steam");
        let game = root.join("steamapps/common/Skyrim Special Edition");
        fs::create_dir_all(root.join("steamapps/compatdata/489830/pfx")).unwrap();
        for p in [
            "steamapps/common/Proton 8.0/proton",
            "steamapps/common/Proton 9.0/proton",
            "compatibilitytools.d/GE-Proton9-20/proton",
        ] {
            touch(&root.join(p));
        }
        let exe = game.join("skse64_loader.exe");
        let roots = vec![root.clone()];
        let p = plan(&sse, &game, &exe, vec!["-x".into()], false, &roots).unwrap();
        assert_eq!(p.program, root.join("steamapps/common/Proton 9.0/proton"));
        assert_eq!(p.args, ["run", &exe.to_string_lossy(), "-x"]);
        assert!(p.env.contains(&(
            "STEAM_COMPAT_DATA_PATH".into(),
            root.join("steamapps/compatdata/489830")
                .to_string_lossy()
                .into()
        )));
        fs::create_dir_all(root.join("config")).unwrap();
        fs::write(
            root.join("config/config.vdf"),
            "\"InstallConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"CompatToolMapping\"\n\t\t{\n\t\t\t\"489830\"\n\t\t\t{\n\t\t\t\t\"name\"\t\t\"GE-Proton9-20\"\n\t\t\t}\n\t\t}\n\t}\n}\n",
        )
        .unwrap();
        assert_eq!(mapping(&root, "489830").as_deref(), Some("GE-Proton9-20"));
        let p = plan(&sse, &game, &exe, vec![], false, &roots).unwrap();
        assert_eq!(
            p.program,
            root.join("compatibilitytools.d/GE-Proton9-20/proton")
        );
        // Plain launch of a Steam game goes through Steam.
        let p = plan(
            &sse,
            &game,
            &game.join("SkyrimSE.exe"),
            vec![],
            true,
            &roots,
        )
        .unwrap();
        assert_eq!(p.args, ["steam://rungameid/489830"]);

        // CrossOver bottle and plain Wine prefix.
        let cx = t.path().join("CrossOver/Bottles/Skyrim");
        let g = cx.join("drive_c/Games/Skyrim");
        let p = plan(&sse, &g, &g.join("SkyrimSE.exe"), vec![], true, &[]).unwrap();
        assert!(p.program.ends_with("CrossOver/bin/wine"));
        assert_eq!(&p.args[..2], ["--bottle", "Skyrim"]);
        let w = t.path().join("pfx/drive_c/Games/Skyrim");
        let p = plan(&sse, &w, &w.join("SkyrimSE.exe"), vec![], true, &[]).unwrap();
        assert_eq!(p.program, PathBuf::from("wine"));
        assert_eq!(
            p.env,
            [(
                "WINEPREFIX".to_string(),
                t.path().join("pfx").to_string_lossy().into_owned()
            )]
        );
        // Nothing to run a Windows exe with.
        let bare = t.path().join("bare");
        assert!(plan(&sse, &bare, &bare.join("SkyrimSE.exe"), vec![], true, &[]).is_err());
        // Native binaries and .app bundles run directly / through `open`.
        let p = plan(
            &sse,
            &bare,
            &bare.join("Game.app"),
            vec!["-a".into()],
            true,
            &[],
        )
        .unwrap();
        assert_eq!(
            (p.program.to_str(), &p.args[1..]),
            (Some("open"), &["--args".to_string(), "-a".into()][..])
        );
        let p = plan(&sse, &bare, &bare.join("game.sh"), vec![], true, &[]).unwrap();
        assert_eq!(p.program, bare.join("game.sh"));
    }
}
