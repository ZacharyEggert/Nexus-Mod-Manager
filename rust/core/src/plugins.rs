//! Plugin load order for Gamebryo/Creation games: read and write it the way each game keeps it.
//! Port of C# `PluginOrderManager` and the ordering rules of `PluginManagementPolicy`.

use crate::game::GameDef;
use crate::{write_durable, IoContext, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plugin {
    /// File name as it is on disk (`Skyrim.esm`).
    pub name: String,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// File modification times are the order; plugins.txt lists the active plugins (Oblivion, FO3, NV).
    Timestamp,
    /// loadorder.txt lists every plugin, plugins.txt the active ones (Skyrim LE).
    SeparateFiles,
    /// plugins.txt lists every plugin, active ones marked `*` (SSE, FO4, Starfield).
    SingleFile,
    /// Plugins.txt inside the plugin folder lists the active plugins in order (Oblivion Remastered).
    PluginDirectory,
}

const EXTENSIONS: &[&str] = &[".esm", ".esp", ".esl"];

/// How one game's plugins are ordered, activated and persisted.
#[derive(Debug, Clone)]
pub struct Policy {
    pub strategy: Strategy,
    /// Folder the plugins live in (`Data`).
    pub dir: PathBuf,
    pub plugins_file: PathBuf,
    pub load_order_file: PathBuf,
    pub marker: String,
    /// Base game masters, in order.
    pub critical: Vec<String>,
    /// Every official plugin: DLC, unmanaged/Creation Club (`.ccc`), policy extras.
    pub official: Vec<String>,
    /// Pinned to the start of their section (masters / non-masters) in this order; can't be moved.
    pub fixed: Vec<String>,
    /// Always active; can't be deactivated.
    pub forced_active: Vec<String>,
    /// Official plugins load without being listed (C# `Fallout4PluginManagement`).
    pub implicit_official: bool,
    /// Official plugins are left out of plugins.txt.
    pub ignore_official: bool,
    /// Comment lines at the top of a single-file plugins.txt.
    pub header: Vec<&'static str>,
    /// Active plugin limit from the definition; 0 = none.
    pub max_active: usize,
}

fn has(list: &[String], name: &str) -> bool {
    list.iter().any(|n| n.eq_ignore_ascii_case(name))
}

fn push_unique(list: &mut Vec<String>, names: impl IntoIterator<Item = String>) {
    for n in names {
        let n = n.trim().to_string();
        if !n.is_empty() && !has(list, &n) {
            list.push(n);
        }
    }
}

/// Master by extension; `.esp` files flagged as masters need the header (#19).
pub fn ext_master(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.ends_with(".esm") || l.ends_with(".esl")
}

fn is_plugin_name(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.len() > 4 && EXTENSIONS.iter().any(|e| l.ends_with(e))
}

/// Non-comment lines of a text file; missing file = none.
fn lines(path: &Path) -> Vec<String> {
    let bytes = fs::read(path).unwrap_or_default();
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(|l| l.trim().trim_start_matches('\u{feff}').to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

impl Policy {
    /// The game's plugin policy; None when it has no plugins file.
    pub fn new(game: &GameDef, game_path: &Path) -> Option<Self> {
        let plugins_file = game.plugins_file(game_path)?;
        let dir = game.plugin_dir(game_path)?;
        let def = game.plugin.as_ref()?;
        let p = def.policy.clone().unwrap_or_default();

        let mut strategy = match p
            .persistence_strategy
            .as_deref()
            .map(str::to_ascii_lowercase)
        {
            Some(s) if s == "timestamp" => Strategy::Timestamp,
            Some(s) if s == "separatefiles" => Strategy::SeparateFiles,
            Some(s) if s == "singlefile" || s == "plugins" || s == "pluginstxt" => {
                Strategy::SingleFile
            }
            Some(s) if s == "plugindirectory" => Strategy::PluginDirectory,
            // C# legacy defaults by mode id.
            _ => match game.mode_id.as_str() {
                "Oblivion" | "Fallout3" | "FalloutNV" => Strategy::Timestamp,
                _ => Strategy::SeparateFiles,
            },
        };
        if p.use_timestamp_order == Some(true) {
            strategy = Strategy::Timestamp;
        }
        if p.load_order_in_plugin_directory == Some(true) {
            strategy = Strategy::PluginDirectory;
        }
        if p.single_file_management == Some(true) {
            strategy = Strategy::SingleFile;
        }
        let load_order_file = match &p.load_order_file_path {
            Some(t) => game.path(t, game_path),
            None => plugins_file.with_file_name("loadorder.txt"),
        };

        let critical = game.ordered_critical_plugin_names.clone();
        let mut unmanaged = vec![];
        push_unique(
            &mut unmanaged,
            game.ordered_official_unmanaged_plugin_names.iter().cloned(),
        );
        for f in &def.official_unmanaged_plugin_list_files {
            let names = lines(&game.path(f, game_path))
                .into_iter()
                .filter(|l| !l.starts_with(';') && !l.contains(['/', '\\']) && l != "..");
            push_unique(&mut unmanaged, names);
        }
        let mut official = vec![];
        push_unique(
            &mut official,
            game.ordered_official_plugin_names.iter().cloned(),
        );
        push_unique(&mut official, p.official_plugins.iter().cloned());
        push_unique(&mut official, unmanaged.iter().cloned());

        let mut fixed = critical.clone();
        push_unique(
            &mut fixed,
            game.ordered_official_plugin_names.iter().cloned(),
        );
        push_unique(&mut fixed, p.fixed_order_plugins.iter().cloned());

        let implicit_official = p.official_plugins_are_implicitly_active.unwrap_or(false);
        let mut forced_active = critical.clone();
        push_unique(&mut forced_active, p.forced_active_plugins.iter().cloned());
        push_unique(&mut forced_active, unmanaged);
        if implicit_official {
            push_unique(&mut forced_active, official.iter().cloned());
        }

        let header = if strategy != Strategy::SingleFile || !implicit_official {
            vec![]
        } else if p.show_starfield_custom_plugins_header == Some(true) {
            vec![
                "# This file is used by Starfield to keep track of your downloaded content.",
                "# Please do not modify this file.",
            ]
        } else {
            vec!["# This file is used by the game to keep track of your downloaded content."]
        };

        Some(Self {
            strategy,
            dir,
            plugins_file,
            load_order_file,
            marker: p.active_marker.unwrap_or_else(|| "*".into()),
            critical,
            official,
            fixed,
            forced_active,
            implicit_official,
            ignore_official: p.ignore_official_plugins.unwrap_or(false),
            header,
            max_active: def.max_allowed_active_plugins_count,
        })
    }

    pub fn is_fixed(&self, name: &str) -> bool {
        has(&self.fixed, name)
    }

    pub fn is_forced_active(&self, name: &str) -> bool {
        has(&self.forced_active, name)
    }

    /// Plugin files in the plugin folder, oldest first (ties by name).
    fn on_disk(&self) -> Vec<(String, SystemTime)> {
        let mut out: Vec<(String, SystemTime)> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                is_plugin_name(&name).then_some(())?;
                let t = e.path().metadata().and_then(|m| m.modified()).ok()?;
                Some((name, t))
            })
            .collect();
        out.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
        });
        out
    }

    /// The current load order: every plugin on disk, in the persisted order, then unlisted ones by age.
    pub fn read(&self) -> Vec<Plugin> {
        let disk = self.on_disk();
        let (order, active): (Vec<String>, Vec<String>) = match self.strategy {
            Strategy::Timestamp => (
                disk.iter().map(|(n, _)| n.clone()).collect(),
                lines(&self.plugins_file),
            ),
            Strategy::SeparateFiles => (lines(&self.load_order_file), lines(&self.plugins_file)),
            Strategy::PluginDirectory => {
                let l = lines(&self.plugins_file);
                (l.clone(), l)
            }
            Strategy::SingleFile => {
                let mut order = vec![];
                let mut active = vec![];
                for l in lines(&self.plugins_file) {
                    match l.strip_prefix(self.marker.as_str()) {
                        Some(n) if !self.marker.is_empty() => {
                            order.push(n.to_string());
                            active.push(n.to_string());
                        }
                        _ => order.push(l),
                    }
                }
                (order, active)
            }
        };
        let mut names: Vec<String> = vec![];
        if self.implicit_official {
            push_unique(&mut names, self.critical.iter().cloned());
            push_unique(&mut names, self.official.iter().cloned());
        }
        push_unique(&mut names, order.into_iter().filter(|n| is_plugin_name(n)));
        // Active but unlisted: right after the previous active plugin (C# `AddMissingElements`).
        let mut after: Option<usize> = None;
        for a in active.iter().filter(|n| is_plugin_name(n)) {
            match names.iter().position(|n| n.eq_ignore_ascii_case(a)) {
                Some(i) => after = Some(i),
                None => {
                    let i = after.map_or(0, |i| i + 1);
                    names.insert(i, a.clone());
                    after = Some(i);
                }
            }
        }
        push_unique(&mut names, disk.iter().map(|(n, _)| n.clone()));

        let mut out: Vec<Plugin> = names
            .iter()
            .filter_map(|n| {
                let (name, _) = disk.iter().find(|(d, _)| d.eq_ignore_ascii_case(n))?;
                Some(Plugin {
                    name: name.clone(),
                    active: has(&active, name),
                })
            })
            .collect();
        self.correct(&mut out, ext_master);
        out
    }

    /// Apply the ordering rules (C# `CorrectStable`, minus dependencies): masters before
    /// non-masters, fixed plugins pinned in order at the start of their section, forced ones active.
    pub fn correct(&self, list: &mut Vec<Plugin>, is_master: impl Fn(&str) -> bool) {
        let mut seen = HashSet::new();
        list.retain(|p| seen.insert(p.name.to_lowercase()));
        let (mut masters, mut rest): (Vec<Plugin>, Vec<Plugin>) =
            list.drain(..).partition(|p| is_master(&p.name));
        for section in [&mut masters, &mut rest] {
            let mut target = 0;
            for f in &self.fixed {
                if let Some(i) = section.iter().position(|p| p.name.eq_ignore_ascii_case(f)) {
                    let p = section.remove(i);
                    section.insert(target, p);
                    target += 1;
                }
            }
        }
        list.extend(masters);
        list.extend(rest);
        for p in list.iter_mut() {
            p.active |= self.is_forced_active(&p.name);
        }
    }

    /// Whether a single-file plugins.txt leaves this plugin out (the game loads it on its own).
    fn implicit(&self, name: &str) -> bool {
        self.strategy == Strategy::SingleFile
            && self.implicit_official
            && (has(&self.critical, name) || (self.ignore_official && has(&self.official, name)))
    }

    /// Persist `plugins` (already corrected) the game's way. Files are rewritten only when they change;
    /// the first write keeps the original as `plugins.nmm.bak` / `loadorder.nmm.bak`.
    pub fn write(&self, plugins: &[Plugin]) -> Result<()> {
        let active: Vec<&str> = plugins
            .iter()
            .filter(|p| p.active && !self.implicit(&p.name))
            .map(|p| p.name.as_str())
            .collect();
        match self.strategy {
            Strategy::Timestamp => {
                self.set_times(plugins)?;
                self.save(&self.plugins_file, "plugins.nmm.bak", &active)
            }
            Strategy::SeparateFiles => {
                let all: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
                self.save(&self.load_order_file, "loadorder.nmm.bak", &all)?;
                self.save(&self.plugins_file, "plugins.nmm.bak", &active)
            }
            Strategy::PluginDirectory => self.save(&self.plugins_file, "plugins.nmm.bak", &active),
            Strategy::SingleFile => {
                let body: Vec<String> = self
                    .header
                    .iter()
                    .map(|h| h.to_string())
                    .chain(plugins.iter().filter(|p| !self.implicit(&p.name)).map(|p| {
                        let m = if p.active { self.marker.as_str() } else { "" };
                        format!("{m}{}", p.name)
                    }))
                    .collect();
                let body: Vec<&str> = body.iter().map(String::as_str).collect();
                self.save(&self.plugins_file, "plugins.nmm.bak", &body)
            }
        }
    }

    fn save(&self, path: &Path, bak: &str, lines: &[&str]) -> Result<()> {
        let body: String = lines.iter().map(|l| format!("{l}\r\n")).collect();
        if fs::read(path).is_ok_and(|old| old == body.as_bytes()) {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).at(dir)?;
            let bak = dir.join(bak);
            if path.exists() && !bak.exists() {
                fs::copy(path, &bak).at(&bak)?;
            }
        }
        write_durable(path, body.as_bytes())
    }

    /// Timestamp order: plugin i gets the first critical plugin's time + i minutes (C# `SetTimestampLoadOrder`).
    fn set_times(&self, plugins: &[Plugin]) -> Result<()> {
        let time = |n: &str| self.dir.join(n).metadata().and_then(|m| m.modified()).ok();
        let base = self
            .critical
            .first()
            .and_then(|c| plugins.iter().find(|p| p.name.eq_ignore_ascii_case(c)))
            .or(plugins.first())
            .and_then(|p| time(&p.name))
            .unwrap_or_else(SystemTime::now);
        for (i, p) in plugins.iter().enumerate() {
            let want = base + Duration::from_secs(60 * i as u64);
            if time(&p.name) != Some(want) {
                let path = self.dir.join(&p.name);
                fs::File::options()
                    .write(true)
                    .open(&path)
                    .and_then(|f| f.set_modified(want))
                    .at(&path)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Game folder with these plugins, file times 1 minute apart in the given order.
    fn game(dir: &Path, plugins: &[&str]) -> PathBuf {
        let data = dir.join("Data");
        fs::create_dir_all(&data).unwrap();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        for (i, p) in plugins.iter().enumerate() {
            let f = fs::File::create(data.join(p)).unwrap();
            f.set_modified(t0 + Duration::from_secs(60 * i as u64))
                .unwrap();
        }
        data
    }

    fn policy(mode: &str, game_path: &Path) -> Policy {
        let mut p = Policy::new(&crate::game::find(mode).unwrap(), game_path).unwrap();
        // Never touch the real AppData from a test.
        p.plugins_file = game_path.join("appdata/plugins.txt");
        p.load_order_file = game_path.join("appdata/loadorder.txt");
        p
    }

    fn names(l: &[Plugin]) -> Vec<String> {
        l.iter()
            .map(|p| format!("{}{}", if p.active { "*" } else { "" }, p.name))
            .collect()
    }

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap()
    }

    #[test]
    fn single_file_with_ccc_and_implicit_official() {
        let t = tempfile::tempdir().unwrap();
        // HearthFires/Dragonborn not installed: skipped.
        game(
            t.path(),
            &[
                "Skyrim.esm",
                "Update.esm",
                "Dawnguard.esm",
                "ccFish.esm",
                "b.esp",
                "a.esp",
                "Late.esm",
                "Loose.esp",
            ],
        );
        fs::write(
            t.path().join("Skyrim.ccc"),
            "ccFish.esm\r\nccMissing.esm\r\n",
        )
        .unwrap();
        let p = policy("SkyrimSE", t.path());
        assert_eq!(p.strategy, Strategy::SingleFile);
        fs::create_dir_all(t.path().join("appdata")).unwrap();
        fs::write(
            &p.plugins_file,
            "# header\r\n*b.esp\r\n*Gone.esp\r\na.esp\r\n*ccFish.esm\r\n*Late.esm\r\n",
        )
        .unwrap();

        let list = p.read();
        // Critical + official first, masters before plugins, unlisted loose file last, missing dropped.
        assert_eq!(
            names(&list),
            [
                "*Skyrim.esm",
                "*Update.esm",
                "*Dawnguard.esm",
                "*ccFish.esm",
                "*Late.esm",
                "*b.esp",
                "a.esp",
                "Loose.esp"
            ]
        );

        p.write(&list).unwrap();
        assert_eq!(
            read(&p.plugins_file),
            "# This file is used by the game to keep track of your downloaded content.\r\n\
             *Late.esm\r\n*b.esp\r\na.esp\r\nLoose.esp\r\n"
        );
        // Original kept once; a second write doesn't replace the backup.
        let bak = t.path().join("appdata/plugins.nmm.bak");
        assert!(read(&bak).starts_with("# header"));
        let mut list = list;
        list.swap(6, 7);
        p.write(&list).unwrap();
        assert!(read(&bak).starts_with("# header"));
        assert_eq!(names(&p.read())[6..], ["Loose.esp", "a.esp"]);
    }

    #[test]
    fn critical_plugins_stay_pinned_and_active() {
        let t = tempfile::tempdir().unwrap();
        game(t.path(), &["a.esp", "Skyrim.esm", "Update.esm"]);
        let p = policy("SkyrimSE", t.path());
        let mut list = vec![
            Plugin {
                name: "Update.esm".into(),
                active: false,
            },
            Plugin {
                name: "a.esp".into(),
                active: true,
            },
            Plugin {
                name: "Skyrim.esm".into(),
                active: false,
            },
        ];
        p.correct(&mut list, ext_master);
        assert_eq!(names(&list), ["*Skyrim.esm", "*Update.esm", "*a.esp"]);
        assert!(p.is_fixed("skyrim.esm") && p.is_forced_active("UPDATE.ESM"));
        assert!(!p.is_fixed("a.esp"));
    }

    #[test]
    fn starfield_header_and_fixed_order() {
        let t = tempfile::tempdir().unwrap();
        game(
            t.path(),
            &["Starfield.esm", "SFBGS004.esm", "SFBGS003.esm", "mine.esm"],
        );
        let p = policy("Starfield", t.path());
        let list = p.read();
        assert_eq!(
            names(&list),
            [
                "*Starfield.esm",
                "*SFBGS003.esm",
                "*SFBGS004.esm",
                "mine.esm"
            ]
        );
        p.write(&list).unwrap();
        assert_eq!(
            read(&p.plugins_file),
            "# This file is used by Starfield to keep track of your downloaded content.\r\n\
             # Please do not modify this file.\r\nmine.esm\r\n"
        );
    }

    #[test]
    fn timestamp_order() {
        let t = tempfile::tempdir().unwrap();
        let data = game(t.path(), &["Oblivion.esm", "b.esp", "a.esp"]);
        let p = policy("Oblivion", t.path());
        assert_eq!(p.strategy, Strategy::Timestamp);
        fs::create_dir_all(t.path().join("appdata")).unwrap();
        fs::write(&p.plugins_file, "Oblivion.esm\r\na.esp\r\n").unwrap();
        let list = p.read();
        assert_eq!(names(&list), ["*Oblivion.esm", "b.esp", "*a.esp"]);

        let swapped = vec![list[0].clone(), list[2].clone(), list[1].clone()];
        p.write(&swapped).unwrap();
        let mtime = |n: &str| data.join(n).metadata().unwrap().modified().unwrap();
        let base = mtime("Oblivion.esm");
        assert_eq!(mtime("a.esp"), base + Duration::from_secs(60));
        assert_eq!(mtime("b.esp"), base + Duration::from_secs(120));
        assert_eq!(names(&p.read()), ["*Oblivion.esm", "*a.esp", "b.esp"]);
        assert_eq!(read(&p.plugins_file), "Oblivion.esm\r\na.esp\r\n");
    }

    #[test]
    fn separate_files_insert_unlisted_active_after_previous_active() {
        let t = tempfile::tempdir().unwrap();
        game(
            t.path(),
            &[
                "Skyrim.esm",
                "Update.esm",
                "x.esp",
                "a.esp",
                "b.esp",
                "c.esp",
            ],
        );
        let p = policy("Skyrim", t.path());
        assert_eq!(p.strategy, Strategy::SeparateFiles);
        fs::create_dir_all(t.path().join("appdata")).unwrap();
        fs::write(
            &p.load_order_file,
            "Skyrim.esm\r\nUpdate.esm\r\na.esp\r\nc.esp\r\n",
        )
        .unwrap();
        fs::write(&p.plugins_file, "Skyrim.esm\r\na.esp\r\nb.esp\r\n").unwrap();
        let list = p.read();
        // b.esp is active but not in loadorder.txt: placed after a.esp. x.esp is unlisted loose.
        assert_eq!(
            names(&list),
            [
                "*Skyrim.esm",
                "*Update.esm",
                "*a.esp",
                "*b.esp",
                "c.esp",
                "x.esp"
            ]
        );
        p.write(&list).unwrap();
        assert_eq!(
            read(&p.load_order_file),
            "Skyrim.esm\r\nUpdate.esm\r\na.esp\r\nb.esp\r\nc.esp\r\nx.esp\r\n"
        );
        assert_eq!(
            read(&p.plugins_file),
            "Skyrim.esm\r\nUpdate.esm\r\na.esp\r\nb.esp\r\n"
        );
    }

    #[test]
    fn plugin_directory_lists_active_in_order() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path();
        let data = game_path.join("OblivionRemastered/Content/Dev/ObvData/Data");
        fs::create_dir_all(&data).unwrap();
        for n in [
            "Oblivion.esm",
            "Knights.esp",
            "AltarGymNavigation.esp",
            "mod.esp",
            "off.esp",
        ] {
            fs::write(data.join(n), "").unwrap();
        }
        fs::write(data.join("Plugins.txt"), "Oblivion.esm\r\nmod.esp\r\n").unwrap();
        let p = Policy::new(&crate::game::find("OblivionRemastered").unwrap(), game_path).unwrap();
        assert_eq!(p.strategy, Strategy::PluginDirectory);
        assert_eq!(p.plugins_file, data.join("Plugins.txt"));
        let list = p.read();
        assert!(names(&list).contains(&"*mod.esp".to_string()), "{list:?}");
        assert!(names(&list).contains(&"off.esp".to_string()), "{list:?}");
        p.write(&list).unwrap();
        let body = read(&data.join("Plugins.txt"));
        assert!(body.starts_with("Oblivion.esm\r\n"), "{body}");
        assert!(
            body.contains("Knights.esp\r\n") && body.contains("mod.esp\r\n"),
            "{body}"
        );
        assert!(!body.contains("off.esp"), "{body}");
    }

    #[cfg(not(windows))]
    #[test]
    fn plugins_file_resolves_inside_proton_prefix() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("steamapps/common/Skyrim Special Edition");
        fs::create_dir_all(&game_path).unwrap();
        let sse = crate::game::find("SkyrimSE").unwrap();
        // No prefix yet: native folders.
        assert!(!sse.plugins_file(&game_path).unwrap().starts_with(t.path()));
        let user = t
            .path()
            .join("steamapps/compatdata/489830/pfx/drive_c/users/steamuser");
        fs::create_dir_all(&user).unwrap();
        assert_eq!(
            sse.plugins_file(&game_path).unwrap(),
            user.join("AppData/Local/Skyrim Special Edition/plugins.txt")
        );
        assert_eq!(
            sse.path(
                r"{PersonalData}\My Games\Skyrim Special Edition",
                &game_path
            ),
            user.join("Documents/My Games/Skyrim Special Edition")
        );
    }
}
