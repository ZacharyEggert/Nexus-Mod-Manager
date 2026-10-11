//! Supported tools (`supportedTools.json` beside a game.json): LOOT, xEdit, BodySlide, FNIS,
//! Nemesis, Pandora, Wrye Bash. Port of C# `DataDrivenSupportedToolsLauncher`: a tool is found in
//! the user's folder for it, else by its discovery rules, else in `executableDirectory`.

use crate::discover::Registry;
use crate::game::GameDef;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub id: String,
    pub name: String,
    /// Key of the user's folder setting; tools sharing a folder share a key (xEdit's QuickAutoClean).
    pub settings_key: Option<String>,
    pub executable_directory: Option<String>,
    pub executable_name: Option<String>,
    pub executable_names: Option<Vec<String>>,
    #[serde(default)]
    pub argument_tokens: Vec<String>,
    #[serde(default)]
    pub discovery_rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    /// `Registry`, `Path` or `GameRelative`.
    pub source: String,
    pub path: Option<String>,
    pub registry_key: Option<String>,
    pub registry_value_name: Option<String>,
    pub path_suffix: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct File {
    #[serde(default)]
    supported_tools: Vec<Tool>,
}

/// The game's supported tools, in definition order.
pub fn of(game: &GameDef) -> Vec<Tool> {
    crate::game::bundle(&game.mode_id)
        .and_then(|b| b.tools)
        .and_then(|t| serde_json::from_str::<File>(t.trim_start_matches('\u{feff}')).ok())
        .map(|f| f.supported_tools)
        .unwrap_or_default()
}

/// C# `IsSafeToolExecutableName`: a plain file name, not a shell.
fn safe_name(n: &str) -> bool {
    const SHELLS: &[&str] = &[
        "cmd.exe",
        "powershell.exe",
        "pwsh.exe",
        "wscript.exe",
        "cscript.exe",
    ];
    !n.trim().is_empty()
        && !n.contains(['/', '\\', ':'])
        && !n.contains("..")
        && !SHELLS.iter().any(|s| n.eq_ignore_ascii_case(s))
}

impl Tool {
    pub fn settings_key(&self) -> &str {
        self.settings_key.as_deref().unwrap_or(&self.id)
    }

    fn names(&self) -> Vec<&str> {
        match &self.executable_names {
            Some(v) => v
                .iter()
                .map(String::as_str)
                .filter(|n| safe_name(n))
                .collect(),
            None => self
                .executable_name
                .as_deref()
                .filter(|n| safe_name(n))
                .into_iter()
                .collect(),
        }
    }

    /// The tool's executable in `dir`, if one of its names is there.
    pub fn in_dir(&self, dir: &Path) -> Option<PathBuf> {
        if !dir.is_dir() {
            return None;
        }
        self.names()
            .into_iter()
            .map(|n| dir.join(n))
            .find(|p| p.is_file())
    }

    /// Where the tool's executable is: `configured` folder, discovery rules, `executableDirectory`.
    pub fn find(
        &self,
        game: &GameDef,
        game_path: &Path,
        configured: Option<&Path>,
        registry: &Registry,
    ) -> Option<PathBuf> {
        if let Some(p) = configured.and_then(|d| self.in_dir(d)) {
            return Some(p);
        }
        let resolve = |t: &str| Some(game.path(t, game_path));
        let rule_dir = |r: &Rule| -> Option<PathBuf> {
            match r.source.as_str() {
                "Registry" => {
                    let v = (registry)(
                        r.registry_key.as_deref()?,
                        r.registry_value_name.as_deref().unwrap_or(""),
                    )?;
                    let v = v.trim().trim_matches('"');
                    let p = PathBuf::from(v);
                    if !p.is_absolute() {
                        return None;
                    }
                    Some(match &r.path_suffix {
                        Some(x) => p.join(x.replace('\\', std::path::MAIN_SEPARATOR_STR)),
                        None => p,
                    })
                }
                "GameRelative" | "Path" => resolve(r.path.as_deref()?),
                _ => None,
            }
        };
        self.discovery_rules
            .iter()
            .filter_map(rule_dir)
            .chain(self.executable_directory.as_deref().and_then(resolve))
            .find_map(|d| self.in_dir(&d))
    }

    /// `argumentTokens` with path tokens expanded (`--game-path={GamePath}`), one argument each.
    pub fn args(&self, game: &GameDef, game_path: &Path) -> Vec<String> {
        self.argument_tokens
            .iter()
            .filter(|t| !t.trim().is_empty())
            .map(|t| {
                if t.contains('{') {
                    // Tokens expand like paths; only the token part becomes native.
                    let mut out = t.clone();
                    for tok in [
                        "{GamePath}",
                        "{LocalApplicationData}",
                        "{PersonalData}",
                        "{Documents}",
                        "{UserGameData}",
                    ] {
                        if out.contains(tok) {
                            let v = game.settings_path(tok, game_path);
                            out = out.replace(tok, &v.to_string_lossy());
                        }
                    }
                    if cfg!(not(windows)) {
                        // `-D:{GamePath}\Data`: the rest of a path token uses `\`.
                        out = out.replace('\\', "/");
                    }
                    out
                } else {
                    t.clone()
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn bundled_tools_found_by_folder_rule_and_setting() {
        let t = tempfile::tempdir().unwrap();
        let sse = crate::game::find("SkyrimSE").unwrap();
        let tools = of(&sse);
        let ids: Vec<&str> = tools.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids[0], "LOOT");
        assert!(ids.contains(&"SSEEdit") && ids.contains(&"Nemesis") && ids.contains(&"BS2"));
        let none: Registry = Box::new(|_, _| None);
        let by = |id: &str| tools.iter().find(|t| t.id == id).unwrap();

        // GameRelative rule: BodySlide in Data/CalienteTools, x64 build preferred.
        let bs = t.path().join("Data/CalienteTools/BodySlide");
        fs::create_dir_all(&bs).unwrap();
        fs::write(bs.join("BodySlide.exe"), "").unwrap();
        assert_eq!(
            by("BS2").find(&sse, t.path(), None, &none),
            Some(bs.join("BodySlide.exe"))
        );
        fs::write(bs.join("BodySlide x64.exe"), "").unwrap();
        assert_eq!(
            by("BS2").find(&sse, t.path(), None, &none),
            Some(bs.join("BodySlide x64.exe"))
        );

        // Registry rule.
        let loot = t.path().join("LOOT");
        fs::create_dir_all(&loot).unwrap();
        fs::write(loot.join("LOOT.exe"), "").unwrap();
        let l = loot.to_string_lossy().into_owned();
        let reg: Registry = Box::new(move |k, v| {
            (k == r"HKEY_LOCAL_MACHINE\SOFTWARE\LOOT" && v == "Installed Path").then(|| l.clone())
        });
        assert_eq!(
            by("LOOT").find(&sse, t.path(), None, &reg),
            Some(loot.join("LOOT.exe"))
        );
        assert_eq!(
            by("LOOT").args(&sse, t.path()),
            ["--game=Skyrim Special Edition"]
        );

        // xEdit has no rule: only the user's folder (shared by QuickAutoClean's settings key).
        assert_eq!(by("SSEEdit").find(&sse, t.path(), None, &none), None);
        fs::write(t.path().join("SSEEdit.exe"), "").unwrap();
        assert_eq!(
            by("SSEEdit").find(&sse, t.path(), Some(t.path()), &none),
            Some(t.path().join("SSEEdit.exe"))
        );
        assert_eq!(by("SSEEditQuickAutoClean").settings_key(), "SSEEdit");
    }

    #[test]
    fn argument_tokens_expand() {
        let t = tempfile::tempdir().unwrap();
        let mut sf = crate::game::find("Starfield").unwrap();
        sf.user_dir = Some(t.path().join("u"));
        let edit = of(&sf).into_iter().find(|t| t.id == "SF1Edit").unwrap();
        let args = edit.args(&sf, Path::new("/g"));
        assert_eq!(args[0], "-sf1");
        if cfg!(not(windows)) {
            assert_eq!(args[1], "-D:/g/Data");
            assert!(
                args[3].ends_with("u/AppData/Local/Starfield/plugins.txt"),
                "{args:?}"
            );
        }
        assert!(
            safe_name("LOOT.exe")
                && !safe_name("cmd.exe")
                && !safe_name("..\\x.exe")
                && !safe_name("a/b.exe")
        );
    }
}
