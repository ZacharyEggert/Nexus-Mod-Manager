//! Plugin load order for Gamebryo/Creation games: read and write it the way each game keeps it.
//! Port of C# `PluginOrderManager` and the ordering rules of `PluginManagementPolicy`.

use crate::game::GameDef;
use crate::{write_durable, IoContext, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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

/// Which load-order slots an active plugin takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// One of the 254-ish `XX` slots.
    Full,
    /// ESL/light (Starfield: small): `FE:XXX`.
    Light,
    /// Starfield medium: `FD:XX`.
    Medium,
}

/// Slots of one class: active plugins of that class get `first..first + max`.
#[derive(Debug, Clone)]
pub struct Space {
    pub class: Class,
    pub first: usize,
    pub max: usize,
    /// C# format string: `{0:X2}`, `FE:{0:X3}`.
    pub format: String,
}

impl Space {
    /// Port of `String.Format(DisplayFormat, index)` for the `{0:Xn}` forms the definitions use.
    fn label(&self, i: usize) -> String {
        let (Some(a), Some(b)) = (self.format.find("{0"), self.format.find('}')) else {
            return format!("{i:02X}");
        };
        let width = self.format[a..b]
            .trim_start_matches("{0:X")
            .parse()
            .unwrap_or(0);
        format!("{}{i:0width$X}{}", &self.format[..a], &self.format[b + 1..])
    }
}

/// What a plugin's header says (esplugin).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Header {
    pub masters: Vec<String>,
    /// Loads in the master section: master flag, `.esm`/`.esl`, or medium.
    pub master: bool,
    pub light: bool,
    pub medium: bool,
    /// Starfield ship blueprint plugin: always loads last.
    pub blueprint: bool,
    pub description: Option<String>,
    /// The header couldn't be read (corrupt, or not a plugin for this game).
    pub unreadable: bool,
}

/// Headers by lowercase file name.
pub type Headers = HashMap<String, Header>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Issue {
    MissingMaster,
    InactiveMaster,
    MasterBelowDependent,
    DependencyCycle,
    UnsupportedClass,
    SlotsExhausted,
}

/// Port of C# `PluginValidationDiagnostic`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub issue: Issue,
    /// Errors block the change that introduces them; warnings are informational.
    pub error: bool,
    pub message: String,
}

/// One plugin as the UI shows it (C# `PluginSnapshotEntry`).
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub name: String,
    pub active: bool,
    pub header: Header,
    /// Slot class while active.
    pub class: Option<Class>,
    /// Load-order index the game gives it (`0A`, `FE:001`); None while inactive or out of slots.
    pub index: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Entry {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.error)
    }
}

fn game_id(mode: &str) -> Option<esplugin::GameId> {
    use esplugin::GameId::*;
    Some(match mode {
        "Oblivion" | "OblivionRemastered" => Oblivion,
        "Fallout3" => Fallout3,
        "FalloutNV" => FalloutNV,
        "Skyrim" | "Enderal" => Skyrim,
        "SkyrimSE" | "SkyrimGOG" | "SkyrimVR" | "EnderalSE" => SkyrimSE,
        "Fallout4" | "Fallout4VR" => Fallout4,
        "Starfield" => Starfield,
        "Morrowind" => Morrowind,
        _ => return None,
    })
}

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
    /// Header format to parse plugins with; None = unknown game, headers unread.
    pub game_id: Option<esplugin::GameId>,
    /// Slot limits per class; a class without a space can't be active.
    pub spaces: Vec<Space>,
    pub masters_first: bool,
    pub validate_dependencies: bool,
    pub blueprint: Vec<String>,
    pub blueprint_prefixes: Vec<String>,
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

/// C# `PluginManagementPolicy` defaults, the definition's active limit as the Full slot count,
/// then the definition's own spaces.
fn spaces(max_active: usize, defs: &[crate::game::AddressSpaceDef]) -> Vec<Space> {
    let space = |class, max, format: &str| Space {
        class,
        first: 0,
        max,
        format: format.into(),
    };
    let mut out = vec![
        space(
            Class::Full,
            if max_active > 0 { max_active } else { 254 },
            "{0:X2}",
        ),
        space(Class::Light, 4096, "FE:{0:X3}"),
        space(Class::Medium, 256, "FD:{0:X2}"),
    ];
    for d in defs {
        let class = match d.address_class.to_ascii_lowercase().as_str() {
            "full" => Class::Full,
            "light" | "small" => Class::Light,
            "medium" => Class::Medium,
            _ => continue,
        };
        out.retain(|s| s.class != class);
        out.push(Space {
            class,
            first: d.first_index,
            max: d.max_count,
            format: d.display_format.clone().unwrap_or_else(|| "{0:X2}".into()),
        });
    }
    out
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
            game_id: game_id(&game.mode_id),
            spaces: spaces(def.max_allowed_active_plugins_count, &p.address_spaces),
            masters_first: p
                .master_plugins_must_load_before_non_masters
                .unwrap_or(true),
            validate_dependencies: p.validate_dependencies.unwrap_or(true),
            blueprint: p.blueprint_plugins.clone(),
            blueprint_prefixes: p.blueprint_plugin_prefixes.clone(),
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
        let headers = self.headers(&out);
        self.correct(&mut out, &headers);
        out
    }

    /// Read one plugin's header.
    pub fn header(&self, name: &str) -> Header {
        let by_ext = ext_master(name);
        let blueprint = has(&self.blueprint, name)
            || self.blueprint_prefixes.iter().any(|p| {
                name.to_ascii_lowercase()
                    .starts_with(&p.to_ascii_lowercase())
            });
        let fallback = Header {
            master: by_ext,
            blueprint,
            unreadable: true,
            ..Header::default()
        };
        let Some(id) = self.game_id else {
            return Header {
                unreadable: false,
                ..fallback
            };
        };
        let mut p = esplugin::Plugin::new(id, &self.dir.join(name));
        if p.parse_file(esplugin::ParseOptions::header_only()).is_err() {
            return fallback;
        }
        let medium = p.is_medium_plugin();
        Header {
            masters: p.masters().unwrap_or_default(),
            master: p.is_master_file() || by_ext || medium,
            light: p.is_light_plugin(),
            medium,
            blueprint: blueprint || p.is_blueprint_plugin(),
            description: p.description().ok().flatten().filter(|d| !d.is_empty()),
            unreadable: false,
        }
    }

    // ponytail: headers are re-read on every load-order read; cache by (size, mtime) if big lists get slow.
    pub fn headers(&self, list: &[Plugin]) -> Headers {
        list.iter()
            .map(|p| (p.name.to_lowercase(), self.header(&p.name)))
            .collect()
    }

    /// Apply the ordering rules (C# `CorrectStable`): masters before non-masters, fixed plugins
    /// pinned in order at the start of their section, masters above their dependents, blueprint
    /// plugins last; forced plugins active.
    pub fn correct(&self, list: &mut Vec<Plugin>, headers: &Headers) {
        let h = |n: &str| headers.get(&n.to_lowercase());
        let mut seen = HashSet::new();
        list.retain(|p| seen.insert(p.name.to_lowercase()));
        let (mut masters, mut rest): (Vec<Plugin>, Vec<Plugin>) = if self.masters_first {
            list.drain(..)
                .partition(|p| h(&p.name).map_or(ext_master(&p.name), |h| h.master))
        } else {
            (std::mem::take(list), vec![])
        };
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
        // Masters above dependents: depth-first, each plugin after its installed masters.
        let all: Vec<Plugin> = masters.into_iter().chain(rest).collect();
        let index: HashMap<String, usize> = all
            .iter()
            .enumerate()
            .map(|(i, p)| (p.name.to_lowercase(), i))
            .collect();
        let mut state = vec![0u8; all.len()]; // 0 new, 1 visiting, 2 placed
                                              // ponytail: recursive; master chains are a few levels deep in practice.
        fn visit(
            i: usize,
            all: &[Plugin],
            index: &HashMap<String, usize>,
            headers: &Headers,
            state: &mut [u8],
            out: &mut Vec<Plugin>,
        ) {
            if state[i] != 0 {
                return; // placed, or a cycle (reported by `check`)
            }
            state[i] = 1;
            let masters = headers
                .get(&all[i].name.to_lowercase())
                .map(|h| h.masters.as_slice())
                .unwrap_or_default();
            for m in masters {
                if let Some(&j) = index.get(&m.to_lowercase()) {
                    visit(j, all, index, headers, state, out);
                }
            }
            state[i] = 2;
            out.push(all[i].clone());
        }
        for i in 0..all.len() {
            visit(i, &all, &index, headers, &mut state, list);
        }
        let (late, mut keep): (Vec<Plugin>, Vec<Plugin>) = list
            .drain(..)
            .partition(|p| h(&p.name).is_some_and(|h| h.blueprint));
        keep.extend(late);
        *list = keep;
        for p in list.iter_mut() {
            p.active |= self.is_forced_active(&p.name);
        }
    }

    /// Slots, mod indices and problems for `list` in order (C# `PluginSnapshotBuilder.Build`).
    pub fn check(&self, list: &[Plugin], headers: &Headers) -> Vec<Entry> {
        let pos: HashMap<String, usize> = list
            .iter()
            .enumerate()
            .map(|(i, p)| (p.name.to_lowercase(), i))
            .collect();
        let mut used: HashMap<Class, usize> = HashMap::new();
        let mut out: Vec<Entry> = list
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let header = headers
                    .get(&p.name.to_lowercase())
                    .cloned()
                    .unwrap_or_else(|| self.header(&p.name));
                let mut diagnostics = vec![];
                let mut d = |issue, error, message: String| {
                    diagnostics.push(Diagnostic {
                        issue,
                        error,
                        message,
                    })
                };
                let class = p.active.then_some(match (header.light, header.medium) {
                    (true, _) => Class::Light,
                    (_, true) => Class::Medium,
                    _ => Class::Full,
                });
                let mut index = None;
                if let Some(c) = class {
                    match self.spaces.iter().find(|s| s.class == c) {
                        None => d(
                            Issue::UnsupportedClass,
                            true,
                            "Plugin class is not supported by this game.".into(),
                        ),
                        Some(s) => {
                            let n = used.entry(c).or_default();
                            if s.max > 0 && *n >= s.max {
                                d(
                                    Issue::SlotsExhausted,
                                    true,
                                    format!("No free load-order slots (limit {}).", s.max),
                                );
                            } else {
                                index = Some(s.label(s.first + *n));
                                *n += 1;
                            }
                        }
                    }
                }
                if self.validate_dependencies {
                    for m in &header.masters {
                        let Some(&j) = pos.get(&m.to_lowercase()) else {
                            d(
                                Issue::MissingMaster,
                                p.active,
                                format!("Missing master: {m}"),
                            );
                            continue;
                        };
                        if p.active && !list[j].active {
                            d(
                                Issue::InactiveMaster,
                                true,
                                format!("Required master is inactive: {m}"),
                            );
                        }
                        if j > i {
                            d(
                                Issue::MasterBelowDependent,
                                true,
                                format!("Required master loads below dependent: {m}"),
                            );
                        }
                    }
                }
                Entry {
                    name: p.name.clone(),
                    active: p.active,
                    header,
                    class,
                    index,
                    diagnostics,
                }
            })
            .collect();
        // Dependency cycles: every plugin on a master path that leads back to itself.
        let n = out.len();
        let edges: Vec<Vec<usize>> = out
            .iter()
            .map(|e| {
                e.header
                    .masters
                    .iter()
                    .filter_map(|m| pos.get(&m.to_lowercase()).copied())
                    .collect()
            })
            .collect();
        for start in 0..n {
            let mut seen = vec![false; n];
            let mut stack = edges[start].clone();
            let mut cyclic = false;
            while let Some(j) = stack.pop() {
                if j == start {
                    cyclic = true;
                    break;
                }
                if !std::mem::replace(&mut seen[j], true) {
                    stack.extend(&edges[j]);
                }
            }
            if cyclic {
                out[start].diagnostics.push(Diagnostic {
                    issue: Issue::DependencyCycle,
                    error: true,
                    message: "Dependency cycle detected.".into(),
                });
            }
        }
        out
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
pub(crate) mod tests {
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
        p.correct(&mut list, &Headers::new());
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

    /// Minimal Skyrim SE / FO4 plugin: TES4 record with HEDR, SNAM and MAST subrecords.
    pub(crate) fn tes4(flags: u32, masters: &[&str], desc: &str) -> Vec<u8> {
        let mut data = vec![];
        let mut sub = |t: &[u8], body: &[u8]| {
            data.extend(t);
            data.extend((body.len() as u16).to_le_bytes());
            data.extend(body);
        };
        let mut hedr = 1.7f32.to_le_bytes().to_vec();
        hedr.extend([0u8; 4]);
        hedr.extend(0x800u32.to_le_bytes());
        sub(b"HEDR", &hedr);
        if !desc.is_empty() {
            sub(b"SNAM", format!("{desc}\0").as_bytes());
        }
        for m in masters {
            sub(b"MAST", format!("{m}\0").as_bytes());
            sub(b"DATA", &[0; 8]);
        }
        let mut out = b"TES4".to_vec();
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(flags.to_le_bytes());
        out.extend([0u8; 12]); // form id, version control, form version + unknown
        out.extend(data);
        out
    }

    fn plugin(data: &Path, name: &str, flags: u32, masters: &[&str]) {
        fs::write(data.join(name), tes4(flags, masters, "")).unwrap();
    }

    fn p(name: &str, active: bool) -> Plugin {
        Plugin {
            name: name.into(),
            active,
        }
    }

    #[test]
    fn headers_drive_master_and_light_flags() {
        let t = tempfile::tempdir().unwrap();
        let data = game(t.path(), &[]);
        fs::write(
            data.join("Desc.esp"),
            tes4(0, &["Skyrim.esm"], "Adds a sword"),
        )
        .unwrap();
        plugin(&data, "Flagged.esp", 0x1, &[]);
        plugin(&data, "Light.esp", 0x200, &[]);
        plugin(&data, "Native.esl", 0, &[]);
        fs::write(data.join("Broken.esm"), "not a plugin").unwrap();
        let p = policy("SkyrimSE", t.path());
        let h = p.header("Desc.esp");
        assert_eq!(h.masters, ["Skyrim.esm"]);
        assert_eq!(h.description.as_deref(), Some("Adds a sword"));
        assert!(!h.master && !h.light && !h.unreadable);
        assert!(p.header("Flagged.esp").master);
        let light = p.header("Light.esp");
        assert!(light.light && !light.master);
        let esl = p.header("Native.esl");
        assert!(esl.light && esl.master);
        // Unreadable: kept, judged by extension.
        let broken = p.header("Broken.esm");
        assert!(broken.unreadable && broken.master);
        // Skyrim LE has no light plugins.
        let le = policy("Skyrim", t.path());
        assert!(!le.header("Light.esp").light);
    }

    #[test]
    fn masters_move_above_dependents_and_blueprints_last() {
        let t = tempfile::tempdir().unwrap();
        let data = game(t.path(), &["Starfield.esm"]);
        plugin(&data, "Patch.esm", 0x1, &["Base.esm"]);
        plugin(&data, "Base.esm", 0x1, &[]);
        plugin(&data, "Uses.esp", 0, &["FlagMaster.esp"]);
        plugin(&data, "FlagMaster.esp", 0x1, &[]);
        plugin(&data, "BlueprintShips-Mine.esm", 0x1, &[]);
        let sse = policy("SkyrimSE", t.path());
        let mut list = vec![
            p("Uses.esp", true),
            p("Patch.esm", true),
            p("FlagMaster.esp", true),
            p("Base.esm", true),
        ];
        let h = sse.headers(&list);
        sse.correct(&mut list, &h);
        assert_eq!(
            names(&list),
            ["*Base.esm", "*Patch.esm", "*FlagMaster.esp", "*Uses.esp"]
        );
        let sf = policy("Starfield", t.path());
        let mut list = vec![
            p("BlueprintShips-Mine.esm", true),
            p("Base.esm", true),
            p("Starfield.esm", true),
        ];
        let h = sf.headers(&list);
        sf.correct(&mut list, &h);
        assert_eq!(
            names(&list),
            ["*Starfield.esm", "*Base.esm", "*BlueprintShips-Mine.esm"]
        );
    }

    #[test]
    fn check_reports_master_problems_cycles_and_slots() {
        let t = tempfile::tempdir().unwrap();
        let data = game(t.path(), &[]);
        plugin(&data, "M.esm", 0x1, &[]);
        plugin(&data, "Off.esm", 0x1, &[]);
        plugin(&data, "Dep.esp", 0, &["M.esm", "Gone.esm", "Off.esm"]);
        plugin(&data, "Idle.esp", 0, &["Gone.esm"]);
        plugin(&data, "Before.esp", 0, &["After.esp"]);
        plugin(&data, "After.esp", 0, &[]);
        plugin(&data, "A.esp", 0, &["B.esp"]);
        plugin(&data, "B.esp", 0, &["A.esp"]);
        plugin(&data, "L1.esp", 0x200, &[]);
        plugin(&data, "L2.esp", 0x200, &[]);
        let mut pol = policy("SkyrimSE", t.path());
        let list = vec![
            p("M.esm", true),
            p("Off.esm", false),
            p("Dep.esp", true),
            p("Idle.esp", false),
            p("Before.esp", true),
            p("After.esp", true),
            p("A.esp", true),
            p("B.esp", true),
            p("L1.esp", true),
            p("L2.esp", true),
        ];
        let h = pol.headers(&list);
        let issues = |e: &Entry| -> Vec<(Issue, bool)> {
            e.diagnostics.iter().map(|d| (d.issue, d.error)).collect()
        };
        let c = pol.check(&list, &h);
        assert_eq!(c[0].index.as_deref(), Some("00"));
        assert_eq!(c[1].index, None);
        assert_eq!(
            issues(&c[2]),
            [(Issue::MissingMaster, true), (Issue::InactiveMaster, true)]
        );
        assert_eq!(c[2].diagnostics[0].message, "Missing master: Gone.esm");
        // Inactive plugin: a missing master is only a warning.
        assert_eq!(issues(&c[3]), [(Issue::MissingMaster, false)]);
        assert_eq!(issues(&c[4]), [(Issue::MasterBelowDependent, true)]);
        assert_eq!(
            issues(&c[6]),
            [
                (Issue::MasterBelowDependent, true),
                (Issue::DependencyCycle, true)
            ]
        );
        assert_eq!(issues(&c[7]), [(Issue::DependencyCycle, true)]);
        assert_eq!(c[8].index.as_deref(), Some("FE:000"));
        assert_eq!(c[9].index.as_deref(), Some("FE:001"));
        assert_eq!(c[5].index.as_deref(), Some("03"));

        // Out of slots: the third full plugin and the second light one get no index.
        pol.spaces
            .iter_mut()
            .for_each(|s| s.max = if s.class == Class::Full { 2 } else { 1 });
        let c = pol.check(&list, &h);
        assert_eq!(c[2].index.as_deref(), Some("01"));
        assert_eq!(issues(&c[4])[0], (Issue::SlotsExhausted, true));
        assert_eq!(c[4].index, None);
        assert_eq!(issues(&c[9]), [(Issue::SlotsExhausted, true)]);
    }

    #[test]
    fn address_spaces_from_definitions() {
        let t = tempfile::tempdir().unwrap();
        let space = |p: &Policy, c| {
            p.spaces
                .iter()
                .find(|s| s.class == c)
                .map(|s| (s.max, s.label(0x1b)))
        };
        let sse = policy("SkyrimSE", t.path());
        assert_eq!(space(&sse, Class::Full), Some((254, "1B".into())));
        assert_eq!(space(&sse, Class::Light), Some((4096, "FE:01B".into())));
        let sf = policy("Starfield", t.path());
        assert_eq!(space(&sf, Class::Full), Some((253, "1B".into())));
        assert_eq!(space(&sf, Class::Medium), Some((256, "FD:1B".into())));
        assert_eq!(space(&sf, Class::Light), Some((4096, "FE:01B".into())));
        // No addressSpaces: maxAllowedActivePluginsCount is the full-slot limit.
        assert_eq!(
            space(&policy("Oblivion", t.path()), Class::Full),
            Some((255, "1B".into()))
        );
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
