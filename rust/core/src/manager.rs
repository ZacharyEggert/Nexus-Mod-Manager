//! Mod library + deployment. Single orchestration point (see VirtualInstallArchitecture.md):
//! callers change intent (`active`, order, profiles) and call `deploy()`, which diffs desired
//! vs deployed links and applies the difference.
//!
//! Layout under `mods_dir`: `VirtualInstall/<id>/…` extracted mod files, `Backup/…` game files
//! displaced by mods, `GameRootInstall/<id>/…` + `GameRootBackup/…` the same for files deployed to
//! the game root instead of the install root (script extenders), `.staging/` scratch, `state.json`, and `deploy.journal` while a deploy
//! is in flight (reconciled against the filesystem on the next `open` if we died mid-deploy).
//! `OrphanedBackup/` holds stale backups `reconcile` set aside (the game replaced the file since).
//! `.pending/<id>` marks a mod being installed or removed; only marked mods' files are ever deleted
//! (see `resolve_pending`), so a lost or stale `state.json` can't wipe the library.

use crate::adjust::{self, Dest};
use crate::fomod::{self, ModuleConfig, Selection};
use crate::game::{has_ext, GameDef};
use crate::{loot, modscript, plugins};
use crate::{write_durable, Error, IoContext, Task};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

type R<T> = crate::Result<T>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModEntry {
    pub id: String,
    pub name: String,
    pub archive: String,
    /// Paths relative to the install root, `/`-separated.
    pub files: Vec<String>,
    /// Paths relative to the game root, outside the install root (e.g. `skse64_loader.exe`).
    /// Only deployed while `game_root` is on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub root_files: Vec<String>,
    /// Per-mod "install to game root" toggle (C# `ModInstallRoot.GameRoot`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub game_root: bool,
    pub active: bool,
    /// Where it came from on Nexus, once known (download, or md5 match).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nexus: Option<crate::nexus::updates::NexusMeta>,
    /// Archive files kept for game-specific bookkeeping, not deployed (BG3 `info.json`, Dragon Age
    /// `Manifest.xml`, `content.xml`): name -> text. See `special`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub special: BTreeMap<String, String>,
    /// Readmes kept from the archive (C# `ReadMeManager`), archive-relative paths; stored under
    /// `ReadMe/<id>/`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readmes: Vec<String>,
    /// `fomod/info.xml` metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<fomod::Info>,
    /// File name of the archive's `fomod/screenshot*`, stored under `Screenshot/<id>/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
}

impl ModEntry {
    /// (path, is game-root) for every file this mod deploys right now, or could (`all`).
    fn deployable(&self, all: bool) -> impl Iterator<Item = (&String, bool)> {
        let root = if all || self.game_root {
            &self.root_files[..]
        } else {
            &[]
        };
        self.files
            .iter()
            .map(|f| (f, false))
            .chain(root.iter().map(|f| (f, true)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Deployed {
    pub path: String,
    pub mod_id: String,
    /// `path` is relative to the game root, not the install root.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub game_root: bool,
    /// The mod's stored path when it differs from `path` (load-order-numbered NMS folders).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
}

/// `deployed` key. Game-root paths get a leading `/` so `x.dll` in the root and in Data don't collide.
fn key(path: &str, game_root: bool) -> String {
    let lower = path.to_lowercase();
    if game_root {
        format!("/{lower}")
    } else {
        lower
    }
}

/// Where one deployed path lives on disk.
struct Place {
    /// Live file in the game.
    target: PathBuf,
    /// Game file displaced by the mod.
    saved: PathBuf,
    /// The mod's own copy.
    src: PathBuf,
    /// Dirs that `target` / `saved` are pruned up to.
    root: PathBuf,
    backup: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Load order: later entries win file conflicts.
    pub mods: Vec<ModEntry>,
    /// `key()` of the relative path -> what is currently linked there.
    pub deployed: BTreeMap<String, Deployed>,
    /// Profile name -> active mod ids.
    pub profiles: BTreeMap<String, Vec<String>>,
    /// Profile name -> plugin load order + active states when saved (games with plugins).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profile_plugins: BTreeMap<String, Vec<plugins::Plugin>>,
    /// Per-game setting: how mod files are placed in the game.
    #[serde(default)]
    pub link_mode: LinkMode,
    /// Mode the current links were made with; a mismatch relinks everything on the next deploy.
    #[serde(default)]
    pub deployed_mode: LinkMode,
    /// What `special::sync` added to game side files (BG3 modsettings.lsx UUIDs, AddIns.xml UIDs).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub special_applied: BTreeMap<String, Vec<String>>,
    /// INI keys mods edited, with who set what (C# install-log INI edits + `IniEdits.xml`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ini_edits: Vec<crate::ini::Edit>,
    /// Mod id -> FOMod wizard choices it was last installed with; kept after uninstall so a
    /// reinstall (or upgrade) can replay them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fomod_choices: BTreeMap<String, Vec<fomod::Choice>>,
    /// Unix seconds of the last Nexus update check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nexus_checked: Option<u64>,
}

/// How deploy places a mod file in the game. Every mode falls back to a copy when its link
/// can't be made (other volume, filesystem without links, Windows without developer mode).
/// Files a game needs as real files (`realFileRequiredExtensions`) are always copied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkMode {
    /// Hardlink, else copy.
    #[default]
    Auto,
    /// Hardlink only; refuses rather than copying, so a full disk can't fill up silently.
    Hardlink,
    /// Symlink (hardlink for `hardlinkRequiredExtensions`), else copy. No disk cost across volumes.
    Symlink,
    /// Always copy: survives the mods folder moving or being deleted.
    Copy,
}

/// What `repair` couldn't settle on its own; left in place for the user to review.
#[derive(Debug, Default, Clone, Serialize, PartialEq)]
pub struct RepairReport {
    /// Stale backups this run set aside under `OrphanedBackup/` (relative to the mods folder):
    /// the game replaced the file after the mod displaced it.
    pub orphaned_backups: Vec<String>,
    /// Game paths holding an unrecorded copy of a mod's file. Could be a mod copy whose record
    /// was lost, or a game file with the same bytes; kept since deleting the latter loses it.
    pub untracked: Vec<String>,
}

/// A plugin as the plugin tab shows it.
#[derive(Debug, Clone, Serialize)]
pub struct PluginRow {
    #[serde(flatten)]
    pub entry: plugins::Entry,
    /// Pinned by the game (base masters, DLC): can't be moved.
    pub locked_order: bool,
    /// Always loaded: can't be deactivated.
    pub locked_active: bool,
    /// Installed mod that deployed it; None for game files and plugins added outside NMM.
    pub owner: Option<String>,
}

/// A LOOT sort to review before applying.
#[derive(Debug, Clone, Serialize)]
pub struct LootPreview {
    pub current: Vec<String>,
    pub sorted: Vec<String>,
    /// Why the masterlist wasn't refreshed, if it wasn't.
    pub note: Option<String>,
}

/// Outcome of a load-order import.
#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    /// Plugins named in the source.
    pub total: usize,
    /// Of those, the ones found and applied.
    pub imported: usize,
    /// Named but not in the game.
    pub unknown: Vec<String>,
}

/// Extracted archive waiting to be installed.
#[derive(Debug, Clone)]
pub struct Staged {
    pub id: String,
    pub archive: PathBuf,
    pub dir: PathBuf,
    /// `Some((base, config))` when the archive ships a FOMod installer.
    pub fomod: Option<(PathBuf, ModuleConfig)>,
    /// Archive path of an unsupported C# script installer (`fomod/script.cs`) when there is no
    /// `ModuleConfig.xml`; `install` then copies the files as they are.
    pub script: Option<String>,
    /// Nexus origin to record on the installed mod.
    pub nexus: Option<crate::nexus::updates::NexusMeta>,
    /// `fomod/info.xml` metadata, else an OMod's `config`.
    pub info: Option<fomod::Info>,
    /// The archive's `fomod/screenshot*`.
    pub screenshot: Option<PathBuf>,
    /// OBMM Mod Script installer (`fomod/script[.txt]`) when there is neither a
    /// `ModuleConfig.xml` nor a C# script; `run_mod_script` must finish it before `install`.
    pub mod_script: Option<ModScript>,
    /// Remembered wizard choices for this mod id (or the one it replaces), mapped onto this config.
    pub choices: Option<Selection>,
    /// Installed mod this replaces in place: itself (reinstall) or an older version (upgrade).
    pub replaces: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModScript {
    /// Folder holding `fomod/`.
    pub base: PathBuf,
    pub code: String,
    /// What the script installs, once it ran to the end.
    pub plan: Option<modscript::Plan>,
}

pub struct Manager {
    pub game: GameDef,
    pub game_path: PathBuf,
    pub mods_dir: PathBuf,
    pub state: State,
    /// Progress + cancel for long operations (extract, install copy, deploy).
    pub task: Task,
}

const IGNORED: &[&str] = &["meta.ini"];
const IGNORED_DIRS: &[&str] = &["__MACOSX"];

fn rel(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| {
            !IGNORED_DIRS
                .iter()
                .any(|d| e.file_name().eq_ignore_ascii_case(d))
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            !IGNORED
                .iter()
                .any(|i| e.file_name().eq_ignore_ascii_case(i))
        })
        .map(|e| e.into_path())
        .collect()
}

/// C# `Readme.IsValidReadme`: a documentation extension, not `script.txt`, not a config file.
fn is_readme(name: &str) -> bool {
    let l = name.to_lowercase();
    l != "script.txt"
        && !l.contains("config")
        && [".txt", ".rtf", ".htm", ".html", ".doc", ".docx", ".pdf"]
            .iter()
            .any(|e| l.ends_with(e))
}

/// Non-empty readmes of an extracted archive outside `fomod/`, `/`-separated and sorted.
/// C# keeps only the first one it meets; this keeps all at the shallowest folder depth, so a
/// top-level readme wins over `interface/translations/*.txt`-style data files further down.
fn readmes(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = files_under(dir)
        .iter()
        .filter(|f| f.metadata().is_ok_and(|m| m.len() > 0))
        .map(|f| rel(f, dir))
        .filter(|r| {
            !r.to_lowercase().split('/').any(|s| s == "fomod")
                && is_readme(r.rsplit('/').next().unwrap())
        })
        .collect();
    let depth = |r: &String| r.matches('/').count();
    let top = found.iter().map(depth).min().unwrap_or(0);
    found.retain(|r| depth(r) == top);
    found.sort_by_key(|r| r.to_lowercase());
    found
}

/// Port of `FOMod.FindPathPrefix`: the archive folder that maps onto the install root.
pub fn find_prefix(dir: &Path, game: &GameDef) -> PathBuf {
    let plugin_dir = game
        .plugin
        .as_ref()
        .and_then(|p| p.plugin_directory_suffix.as_deref())
        .map(|s| s.rsplit(['\\', '/']).next().unwrap_or(s).to_string());
    let mut cur = dir.to_path_buf();
    loop {
        let Ok(rd) = fs::read_dir(&cur) else {
            return dir.to_path_buf();
        };
        let (mut dirs, mut files) = (vec![], vec![]);
        for e in rd.filter_map(Result::ok) {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.path().is_dir() {
                if !IGNORED_DIRS.iter().any(|d| name.eq_ignore_ascii_case(d)) {
                    dirs.push((name, e.path()));
                }
            } else {
                files.push(name);
            }
        }
        let is_stop = |n: &str| {
            n.eq_ignore_ascii_case("fomod")
                || game.stop_folders.iter().any(|s| s.eq_ignore_ascii_case(n))
        };
        if dirs.iter().any(|(n, _)| is_stop(n)) || files.iter().any(|f| game.is_plugin(f)) {
            return cur;
        }
        if let Some((_, p)) = dirs.iter().find(|(n, _)| {
            plugin_dir
                .as_deref()
                .is_some_and(|d| d.eq_ignore_ascii_case(n))
        }) {
            return p.clone();
        }
        match (dirs.len(), files.len()) {
            (1, 0) => cur = dirs.remove(0).1,
            _ => return dir.to_path_buf(),
        }
    }
}

/// Files beside the archive's Data folder, as (source, game-root path), when `prefix` is that
/// Data folder; plus whether the mod's game-root toggle should default on.
// ponytail: default-on when a sibling is exe/dll/asi (script extenders, ENB); the user flips it otherwise.
fn game_root_files(dir: &Path, prefix: &Path, game: &GameDef) -> (Vec<(PathBuf, String)>, bool) {
    let is_data = |p: &Path| {
        game.data_dir_name()
            .zip(p.file_name())
            .is_some_and(|(d, n)| n.eq_ignore_ascii_case(d))
    };
    let Some(base) = prefix.parent().filter(|_| {
        game.mod_install.supports_game_root_install && prefix != dir && is_data(prefix)
    }) else {
        return (vec![], false);
    };
    let files: Vec<_> = files_under(base)
        .into_iter()
        .filter(|f| !f.starts_with(prefix))
        .collect();
    let binary = files.iter().any(|f| {
        has_ext(
            &f.to_string_lossy(),
            &[".exe".into(), ".dll".into(), ".asi".into()],
        )
    });
    let files = files
        .into_iter()
        .map(|f| (rel(&f, base), f))
        .filter(|(r, _)| !r.to_lowercase().starts_with("fomod/"))
        .map(|(r, f)| (f, r))
        .collect();
    (files, binary)
}

fn symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(src, dst);
    #[cfg(windows)]
    return std::os::windows::fs::symlink_file(src, dst);
}

/// Place `src` at `dst` per `mode`; see `LinkMode`. `real_copy` / `need_hard` come from the game def.
fn place_file(src: &Path, dst: &Path, mode: LinkMode, real_copy: bool, need_hard: bool) -> R<()> {
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p).at(p)?;
    }
    // A dangling symlink (its mod's files gone) isn't game data; clear it so the new link fits.
    if fs::symlink_metadata(dst).is_ok_and(|m| m.is_symlink()) && !dst.exists() {
        fs::remove_file(dst).at(dst)?;
    }
    let fail = |e: std::io::Error| format!("deploy {} -> {}: {e}", src.display(), dst.display());
    let linked = match mode {
        _ if real_copy => false,
        LinkMode::Copy => false,
        LinkMode::Hardlink => {
            fs::hard_link(src, dst).map_err(|e| {
                fail(e) + " (hardlink mode: mods folder must be on the game's drive)"
            })?;
            true
        }
        LinkMode::Symlink if !need_hard => symlink(src, dst).is_ok(),
        LinkMode::Symlink | LinkMode::Auto => fs::hard_link(src, dst).is_ok(),
    };
    if !linked {
        copy_atomic(src, dst).map_err(fail)?;
    }
    Ok(())
}

/// Scratch name for a copy in flight; `reconcile` deletes strays.
fn tmp_path(p: &Path) -> PathBuf {
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".nmm-tmp");
    p.with_file_name(name)
}

/// Copy via tmp + rename, so a kill mid-copy never leaves a torn `dst`.
fn copy_atomic(src: &Path, dst: &Path) -> std::io::Result<()> {
    let tmp = tmp_path(dst);
    fs::copy(src, &tmp)?;
    crash_point();
    fs::rename(&tmp, dst)
}

fn move_file(src: &Path, dst: &Path) -> R<()> {
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p).at(p)?;
    }
    if fs::rename(src, dst).is_err() {
        copy_atomic(src, dst).at(dst)?;
        fs::remove_file(src).at(src)?;
    }
    Ok(())
}

/// Lower-cased path -> (deployed before, deployed after) for every path a deploy changes.
type Journal = BTreeMap<String, (Option<Deployed>, Option<Deployed>)>;

/// Key -> (link that locates the path on disk, links that may be live there, likeliest first).
type Candidates = BTreeMap<String, (Deployed, Vec<Deployed>)>;

fn candidates(journal: &Journal) -> Candidates {
    let pick = |(old, new): &(Option<Deployed>, Option<Deployed>)| {
        let all: Vec<Deployed> = [new, old].into_iter().flatten().cloned().collect();
        (all[0].clone(), all)
    };
    journal.iter().map(|(k, v)| (k.clone(), pick(v))).collect()
}

// ponytail: byte compare, not inode; a game file identical to a mod's reads as deployed.
fn same_content(a: &Path, b: &Path) -> bool {
    matches!((fs::metadata(a), fs::metadata(b)), (Ok(x), Ok(y)) if x.len() == y.len())
        && fs::read(a).ok() == fs::read(b).ok()
}

/// `link` is a hardlink or symlink to `src`.
// ponytail: Windows hardlinks read as distinct (no stable file-index API); repair then leaves them untracked.
fn same_file(src: &Path, link: &Path) -> bool {
    if fs::read_link(link).is_ok_and(|t| t == src) {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        matches!((fs::metadata(src), fs::symlink_metadata(link)), (Ok(a), Ok(b)) if (a.dev(), a.ino()) == (b.dev(), b.ino()))
    }
    #[cfg(not(unix))]
    false
}

#[cfg(test)]
thread_local!(static CRASH_AFTER: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) });

/// Test hook: panics once `CRASH_AFTER` filesystem steps have run, simulating a kill.
fn crash_point() {
    #[cfg(test)]
    CRASH_AFTER.with(|c| match c.get() {
        0 => panic!("simulated crash"),
        n => c.set(n - 1),
    });
}

/// `base/rel`, reusing the casing of whatever already exists on disk, so `Textures/x.dds` lands
/// in the game's `textures/` on case-sensitive volumes. Components past the first missing one
/// keep `rel`'s casing. No-op cost on case-insensitive volumes: one stat when the path exists.
// ponytail: read_dir per missed component, uncached; cache dir listings per deploy if it shows up in profiles.
pub(crate) fn resolve_ci(base: &Path, rel: &str) -> PathBuf {
    let exact = base.join(rel);
    if exact.exists() {
        return exact;
    }
    let mut cur = base.to_path_buf();
    let mut parts = rel.split('/').filter(|c| !c.is_empty());
    for part in parts.by_ref() {
        let found = if cur.join(part).exists() {
            Some(part.into())
        } else {
            fs::read_dir(&cur).ok().and_then(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name())
                    .find(|n| n.to_string_lossy().eq_ignore_ascii_case(part))
            })
        };
        match found {
            Some(name) => cur.push(name),
            None => {
                cur.push(part);
                break;
            }
        }
    }
    cur.extend(parts);
    cur
}

/// `base/rel` exists, matching each component case-insensitively; `rel` may use `\\`.
pub(crate) fn exists_ci(base: &Path, rel: &str) -> bool {
    resolve_ci(base, &rel.replace('\\', "/")).exists()
}

/// Remove now-empty parent dirs up to (not including) `stop`.
fn prune(mut dir: &Path, stop: &Path) {
    while dir != stop && dir.starts_with(stop) && fs::remove_dir(dir).is_ok() {
        match dir.parent() {
            Some(p) => dir = p,
            None => break,
        }
    }
}

impl Manager {
    pub fn open(game: GameDef, game_path: PathBuf, mods_dir: PathBuf) -> R<Self> {
        fs::create_dir_all(&mods_dir).at(&mods_dir)?;
        let load = |f: &str| serde_json::from_str(&fs::read_to_string(mods_dir.join(f)).ok()?).ok();
        let state = match load("state.json").or_else(|| load("state.json.bak")) {
            Some(s) => s,
            None if mods_dir.join("state.json").exists() => {
                return Err("state.json and its backup are unreadable".into())
            }
            None => State::default(),
        };
        let mut m = Self {
            game,
            game_path,
            mods_dir,
            state,
            task: Task::default(),
        };
        if let Ok(s) = fs::read_to_string(m.journal_path()) {
            tracing::warn!(mods_dir = %m.mods_dir.display(), "unfinished deploy found; recovering");
            match serde_json::from_str(&s) {
                Ok(journal) => m.reconcile(&candidates(&journal))?,
                Err(e) => {
                    tracing::warn!("unreadable deploy.journal ({e}); scanning the game instead");
                    m.scan()?;
                }
            }
            m.finish(&HashSet::new())?;
            // The kill interrupted a change (or its rollback) before state was saved: deploy the
            // saved intent so the game holds all of it or none, not half a mod until a manual redeploy.
            // ponytail: a failure here just leaves in_sync() false; the UI offers Redeploy.
            if let Err(e) = m.deploy() {
                tracing::error!("redeploy after recovery failed: {e}");
            }
        }
        // ponytail: a failure leaves the marker; the next open retries.
        if let Err(e) = m.resolve_pending() {
            tracing::warn!("cleanup of uninstalled mods failed, retrying next open: {e}");
        }
        Ok(m)
    }

    /// Mark `id`'s stored files as possibly orphaned until the install/remove settles.
    fn mark_pending(&self, id: &str) -> R<()> {
        let dir = self.mods_dir.join(".pending");
        fs::create_dir_all(&dir).at(&dir)?;
        fs::write(dir.join(id), b"").at(dir.join(id))
    }

    /// Settle every marked mod: delete its stored files when it's neither listed nor deployed,
    /// then drop the marker. Unmarked files are never deleted, whatever `state.json` says.
    fn resolve_pending(&self) -> R<()> {
        for e in fs::read_dir(self.mods_dir.join(".pending"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
        {
            let id = e.file_name().to_string_lossy().into_owned();
            let used = self.state.mods.iter().any(|m| m.id == id)
                || self.state.deployed.values().any(|d| d.mod_id == id);
            if !used {
                for dir in [
                    self.virtual_dir(&id),
                    self.mods_dir.join("GameRootInstall").join(&id),
                    self.readme_dir(&id),
                    self.screenshot_dir(&id),
                ] {
                    match fs::remove_dir_all(&dir) {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                            return Err(Error::Io {
                                path: dir,
                                source: e,
                            })
                        }
                        _ => {}
                    }
                }
                tracing::info!(id, "deleted stored files of uninstalled mod");
            }
            fs::remove_file(e.path()).at(e.path())?;
        }
        Ok(())
    }

    pub fn save(&self) -> R<()> {
        let (path, bak) = (
            self.mods_dir.join("state.json"),
            self.mods_dir.join("state.json.bak"),
        );
        // Previous state survives as .bak: a hardlink to the old, already-flushed inode, no copy.
        let _ = fs::remove_file(&bak);
        let _ = fs::hard_link(&path, &bak);
        write_durable(
            &path,
            &serde_json::to_vec_pretty(&self.state).map_err(|e| e.to_string())?,
        )
    }

    fn virtual_dir(&self, id: &str) -> PathBuf {
        self.mods_dir.join("VirtualInstall").join(id)
    }

    fn readme_dir(&self, id: &str) -> PathBuf {
        self.mods_dir.join("ReadMe").join(id)
    }

    /// Copy the archive's readmes to `ReadMe/<id>/`; returns the ones kept. A failure only loses
    /// readmes, never the install (C# `VerifyReadMeFile` swallows errors too).
    fn keep_readmes(&self, staged: &Staged) -> Vec<String> {
        let found = readmes(&staged.dir);
        let dir = self.readme_dir(&staged.id);
        found
            .into_iter()
            .filter(|r| {
                let to = dir.join(r);
                let kept = fs::create_dir_all(to.parent().unwrap())
                    .and_then(|()| fs::copy(staged.dir.join(r), &to));
                if let Err(e) = &kept {
                    tracing::warn!(id = staged.id, readme = r, "keeping readme failed: {e}");
                }
                kept.is_ok()
            })
            .collect()
    }

    /// Copy the archive's screenshot to `Screenshot/<id>/`; a failure only loses the screenshot.
    fn keep_screenshot(&self, staged: &Staged) -> Option<String> {
        let src = staged.screenshot.as_ref()?;
        let name = src.file_name()?.to_string_lossy().into_owned();
        let dir = self.screenshot_dir(&staged.id);
        fs::create_dir_all(&dir)
            .and_then(|()| fs::copy(src, dir.join(&name)))
            .inspect_err(|e| tracing::warn!(id = staged.id, "keeping screenshot failed: {e}"))
            .ok()?;
        Some(name)
    }

    /// Where mod `id`'s readme `name` (one of its `readmes`) is stored.
    pub fn readme_path(&self, id: &str, name: &str) -> Option<PathBuf> {
        let m = self.state.mods.iter().find(|m| m.id == id)?;
        m.readmes
            .iter()
            .any(|r| r == name)
            .then(|| self.readme_dir(id).join(name))
    }

    fn screenshot_dir(&self, id: &str) -> PathBuf {
        self.mods_dir.join("Screenshot").join(id)
    }

    /// Where mod `id`'s kept screenshot is.
    pub fn screenshot_path(&self, id: &str) -> Option<PathBuf> {
        let m = self.state.mods.iter().find(|m| m.id == id)?;
        Some(self.screenshot_dir(id).join(m.screenshot.as_ref()?))
    }

    pub fn install_root(&self) -> PathBuf {
        self.game.install_root(&self.game_path)
    }

    fn place(&self, d: &Deployed) -> Place {
        let (root, backup, store) = if d.game_root {
            (
                self.game.secondary_root(&self.game_path),
                self.mods_dir.join("GameRootBackup"),
                self.mods_dir.join("GameRootInstall").join(&d.mod_id),
            )
        } else {
            (
                self.install_root(),
                self.mods_dir.join("Backup"),
                self.virtual_dir(&d.mod_id),
            )
        };
        let src = store.join(d.src.as_deref().unwrap_or(&d.path));
        let target = resolve_ci(&root, &d.path);
        // Backup mirrors the game's casing, and is itself looked up case-insensitively.
        let saved = resolve_ci(&backup, &rel(&target, &root));
        Place {
            target,
            saved,
            src,
            root,
            backup,
        }
    }

    /// What FOMod conditions check: plugin states, game and script extender versions.
    pub fn fomod_env(&self) -> fomod::Env {
        let plugins = plugins::Policy::new(&self.game, &self.game_path).map(|p| p.read());
        let root = self.install_root();
        let version = |exes: &[String]| {
            exes.iter().find_map(|e| {
                fomod::exe_version(&resolve_ci(&self.game_path, &e.replace('\\', "/")))
            })
        };
        let extenders = self
            .game
            .gamebryo
            .as_ref()
            .map(|g| &g.script_extender_executables[..]);
        fomod::Env {
            file_state: Box::new(move |name| {
                let name = name.replace('\\', "/");
                // C# `PluginCondition`: a plugin is Active / Inactive per the load order, Missing
                // when not on disk.
                if let Some(list) = plugins.as_ref().filter(|_| plugins::is_plugin_name(&name)) {
                    return match list.iter().find(|p| p.name.eq_ignore_ascii_case(&name)) {
                        Some(p) if p.active => "Active",
                        Some(_) => "Inactive",
                        None => "Missing",
                    }
                    .into();
                }
                // Other files (and games without plugins): present counts as Active.
                if resolve_ci(&root, &name).exists() {
                    "Active"
                } else {
                    "Missing"
                }
                .into()
            }),
            game: version(&self.game.game_executables),
            extender: extenders.and_then(version),
        }
    }

    // ---------- install ----------

    pub fn stage(&self, archive: &Path) -> R<Staged> {
        let stem = archive
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("bad archive name")?;
        let id: String = stem
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || " -_.()".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        // "." / ".." would make VirtualInstall/<id> the store itself (and `remove` delete it).
        if id.trim_matches('.').is_empty() {
            return Err("bad archive name".into());
        }
        let dir = self.mods_dir.join(".staging").join(&id);
        let _ = fs::remove_dir_all(&dir);
        // Only Oblivion takes OMods (C# `OblivionGameMode` formats).
        let omod_game = self.game.mode_id == "Oblivion";
        let is_omod = archive
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("omod"));
        if is_omod && !omod_game {
            return Err("OMod archives can only be installed for Oblivion".into());
        }
        let mut omod_info = None;
        let fomod = crate::archive::extract(archive, &dir, &self.task).and_then(|()| {
            if omod_game {
                omod_info = crate::omod::convert(&dir, &self.task)?;
            }
            match fomod::locate(&dir) {
                Some((base, cfg)) => Ok(Some((base, fomod::parse(&fs::read(&cfg).at(&cfg)?)?))),
                None => Ok(None),
            }
        });
        let mut fomod = match fomod {
            Ok(f) => f,
            Err(e) => {
                let _ = fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        let screenshot = fomod::locate_screenshot(&dir);
        let mut choices = None;
        if let Some((base, cfg)) = &mut fomod {
            if let Some(why) = cfg.unmet(&self.fomod_env()) {
                let _ = fs::remove_dir_all(&dir);
                return Err(why.into());
            }
            // C# shows the screenshot when the config names no header image.
            if cfg.image.is_none() {
                cfg.image = screenshot
                    .as_ref()
                    .filter(|s| s.starts_with(&*base))
                    .map(|s| rel(s, base));
            }
            choices = self.saved_choices(cfg, &id);
        }
        let script = fomod
            .is_none()
            .then(|| fomod::locate_script(&dir))
            .flatten();
        let mod_script = match (&fomod, &script, modscript::locate(&dir)) {
            (None, None, Some(p)) => Some(ModScript {
                base: p.parent().unwrap().parent().unwrap().to_path_buf(),
                code: crate::ini::decode(&fs::read(&p).at(&p)?),
                plan: None,
            }),
            _ => None,
        };
        Ok(Staged {
            script: script.map(|p| rel(&p, &dir)),
            mod_script,
            info: fomod::locate_info(&dir)
                .and_then(|p| fomod::parse_info(&fs::read(p).ok()?))
                .or(omod_info),
            screenshot,
            choices,
            replaces: self
                .state
                .mods
                .iter()
                .any(|m| m.id == id)
                .then(|| id.clone()),
            id,
            archive: archive.to_path_buf(),
            dir,
            fomod,
            nexus: None,
        })
    }

    /// What a Mod Script in `base` sees of the mod and this game.
    pub fn mod_script_host(&self, base: &Path) -> modscript::Host {
        let env = self.fomod_env();
        let files = files_under(base)
            .iter()
            .map(|f| rel(f, base))
            .filter(|r| !r.to_lowercase().starts_with("fomod"))
            .collect();
        let plugins = plugins::Policy::new(&self.game, &self.game_path)
            .map(|p| p.read().into_iter().map(|p| (p.name, p.active)).collect())
            .unwrap_or_default();
        modscript::Host {
            files,
            base: base.to_path_buf(),
            install_root: self.install_root(),
            plugin_exts: self.game.plugin_extensions.clone(),
            plugins,
            game: env.game,
            extender: env.extender,
            game_ini: self.settings_file(None),
            renderer: self.game.gamebryo.as_ref().map(|_| {
                self.game
                    .settings_path("{UserGameData}\\RendererInfo.txt", &self.game_path)
            }),
            gamebryo: ["Oblivion", "Morrowind"].contains(&&self.game.mode_id[..]),
        }
    }

    /// Run the staged Mod Script with `answers` to its questions so far: the next question, or
    /// None once it finished (its plan is then kept for `install`).
    pub fn run_mod_script(
        &self,
        s: &mut Staged,
        answers: &[modscript::Answer],
    ) -> R<Option<modscript::Prompt>> {
        let ms = s.mod_script.as_mut().ok_or("not a Mod Script install")?;
        match modscript::run(&ms.code, &self.mod_script_host(&ms.base), answers)? {
            modscript::Outcome::Ask(p) => Ok(Some(p)),
            modscript::Outcome::Done(plan) => {
                ms.plan = Some(plan);
                Ok(None)
            }
        }
    }

    /// A Mod Script's plugin changes, after its plugins are deployed: relative load orders
    /// (C# `SetRelativeLoadOrder`: each listed plugin found above the previous one moves just
    /// below it), then `UncheckEsp`. Best effort: a change that breaks the load order is skipped.
    fn apply_script_plugins(&self, plan: &modscript::Plan) {
        if plan.uncheck.is_empty() && plan.orders.is_empty() {
            return;
        }
        let Some(policy) = plugins::Policy::new(&self.game, &self.game_path) else {
            return;
        };
        let mut list = policy.read();
        let at = |list: &[plugins::Plugin], n: &str| {
            let n = n.rsplit(['/', '\\']).next().unwrap_or(n);
            list.iter().position(|p| p.name.eq_ignore_ascii_case(n))
        };
        for order in &plan.orders {
            let mut cur = None;
            for name in order {
                let Some(i) = at(&list, name) else { continue };
                cur = match cur {
                    Some(c) if i < c => {
                        let p = list.remove(i);
                        list.insert(c, p);
                        Some(c)
                    }
                    _ => Some(i),
                };
            }
        }
        for n in &plan.uncheck {
            if let Some(i) = at(&list, n) {
                list[i].active = false;
            }
        }
        if let Err(e) = self.set_plugins(&list) {
            tracing::warn!("Mod Script plugin changes not applied: {e}");
        }
    }

    fn saved_choices(&self, cfg: &ModuleConfig, id: &str) -> Option<Selection> {
        let c = cfg.selection(self.state.fomod_choices.get(id)?);
        (!c.is_empty()).then_some(c)
    }

    /// An installed older version of `s` to offer upgrading (C# `ModMatcher.FindAlternateVersion`):
    /// same Nexus mod, both versions known and different, and the same file by id, update chain
    /// or name.
    // ponytail: first match; C# prefers the largest archive when several match.
    pub fn older_version(&self, s: &Staged) -> Option<String> {
        let n = s.nexus.as_ref()?;
        let known = |v: &str| !v.trim().is_empty();
        let hit = |o: &crate::nexus::updates::NexusMeta| {
            o.domain == n.domain
                && o.mod_id == n.mod_id
                && known(&o.version)
                && known(&n.version)
                && !o.version.trim().eq_ignore_ascii_case(n.version.trim())
                && (o.file_id == n.file_id
                    || o.update.as_ref().is_some_and(|u| u.file_id == n.file_id)
                    || o.file_name.eq_ignore_ascii_case(&n.file_name))
        };
        let m = self
            .state
            .mods
            .iter()
            .find(|m| m.id != s.id && m.nexus.as_ref().is_some_and(hit))?;
        Some(m.id.clone())
    }

    /// Make `s` replace installed mod `old` in place (C# `ModUpgrader`), offering the FOMod
    /// choices `old` was installed with.
    pub fn set_replaces(&self, s: &mut Staged, old: &str) {
        if let Some(c) = s
            .fomod
            .as_ref()
            .and_then(|(_, cfg)| self.saved_choices(cfg, old))
        {
            s.choices = Some(c);
        }
        s.replaces = Some(old.into());
    }

    /// Install a staged archive. `sel` is required when the archive is a FOMod. With `replaces`,
    /// the new files take the old mod's place: its load-order slot, enabled state and profiles;
    /// paths both versions deploy keep their plugin position and state, files only the old one
    /// had are removed (C# `ModFileUpgradeInstaller`).
    pub fn install(&mut self, staged: Staged, sel: Option<&Selection>) -> R<()> {
        let slot = match &staged.replaces {
            Some(r) => Some(
                self.state
                    .mods
                    .iter()
                    .position(|m| &m.id == r)
                    .ok_or_else(|| format!("'{r}' is not installed"))?,
            ),
            None => None,
        };
        if staged.replaces.as_ref() != Some(&staged.id)
            && self.state.mods.iter().any(|m| m.id == staged.id)
        {
            return Err(format!("'{}' is already installed; remove it first", staged.id).into());
        }
        let prev = slot.map(|i| self.state.mods[i].clone());
        let same = prev.as_ref().is_some_and(|p| p.id == staged.id);
        // C# names the mod from info.xml.
        let name = match (&staged.info, &staged.fomod, sel) {
            (Some(i), _, _) if !i.name.is_empty() => i.name.clone(),
            (_, Some((_, cfg)), Some(_)) if !cfg.name.is_empty() => cfg.name.clone(),
            _ => staged.id.clone(),
        };
        let profile = self.game.mod_install.path_adjustment_profile;
        // key(dest) -> (source, dest, game_root); later inserts win, matching FOMod priority order.
        let mut map: BTreeMap<String, (PathBuf, String, bool)> = BTreeMap::new();
        let mut profile_root = false;
        let mut insert = |src: PathBuf, dest: String, game_root: bool| {
            if !IGNORED.iter().any(|i| {
                dest.rsplit('/')
                    .next()
                    .unwrap_or("")
                    .eq_ignore_ascii_case(i)
            }) {
                map.insert(key(&dest, game_root), (src, dest, game_root));
            }
        };
        // (source, dest, game_root) before the game's path adjustment profile, which may need all of them.
        let mut raw: Vec<(PathBuf, String, bool)> = vec![];
        let mut put =
            |src: PathBuf, dest: String, game_root: bool| raw.push((src, dest, game_root));
        let mut game_root = false;
        let script_plan = match &staged.mod_script {
            Some(ms) => Some(
                ms.plan
                    .clone()
                    .ok_or("the Mod Script hasn't run to the end")?,
            ),
            None => None,
        };
        match (&staged.fomod, sel) {
            (None, _) if script_plan.is_some() => {
                let ms = staged.mod_script.as_ref().unwrap();
                let out = staged.dir.join(".modscript");
                for (i, (dest, src)) in script_plan.as_ref().unwrap().files.values().enumerate() {
                    let from = match src {
                        modscript::Src::Mod(p) => ms.base.join(p),
                        modscript::Src::Bytes(b) => {
                            let f = out.join(i.to_string());
                            fs::create_dir_all(&out).at(&out)?;
                            fs::write(&f, b).at(&f)?;
                            f
                        }
                    };
                    put(from, dest.clone(), false);
                }
            }
            (Some((base, cfg)), Some(sel)) => {
                // Case-insensitive index of the archive: FOMod paths rarely match on-disk case.
                let index: HashMap<String, PathBuf> = walkdir::WalkDir::new(base)
                    .into_iter()
                    .filter_map(Result::ok)
                    .map(|e| (rel(e.path(), base).to_lowercase(), e.into_path()))
                    .collect();
                for f in cfg.files(sel, &self.fomod_env()) {
                    let key = f.source.trim_matches('/').to_lowercase();
                    let src = index
                        .get(&key)
                        .ok_or_else(|| format!("FOMod references missing file '{}'", f.source))?;
                    let dest = f.destination.trim_matches('/');
                    if f.is_folder {
                        for file in files_under(src) {
                            let r = rel(&file, src);
                            put(
                                file,
                                if dest.is_empty() {
                                    r
                                } else {
                                    format!("{dest}/{r}")
                                },
                                false,
                            );
                        }
                    } else {
                        let d = if dest.is_empty() || dest == "." {
                            f.source.rsplit('/').next().unwrap().to_string()
                        } else {
                            dest.to_string()
                        };
                        put(src.clone(), d, false);
                    }
                }
            }
            (Some(_), None) => return Err("FOMod selection required".into()),
            (None, _) => {
                let prefix = find_prefix(&staged.dir, &self.game);
                for file in files_under(&prefix) {
                    let r = rel(&file, &prefix);
                    if !r.to_lowercase().starts_with("fomod/") {
                        put(file, r, false);
                    }
                }
                let (root, on) = game_root_files(&staged.dir, &prefix, &self.game);
                game_root = on;
                for (file, r) in root {
                    put(file, r, true);
                }
            }
        }

        // Install-root paths go through the game's path adjustment profile.
        let pre: Vec<String> = raw.iter().filter(|r| !r.2).map(|r| r.1.clone()).collect();
        let src_of = |d: &str| raw.iter().find(|r| !r.2 && r.1 == d).map(|r| r.0.clone());
        let content_name = match adjust::prepare(profile, &pre, &|d| fs::read(src_of(d)?).ok()) {
            Ok(c) => c,
            Err(e) => {
                let _ = fs::remove_dir_all(&staged.dir);
                return Err(e.into());
            }
        };
        let archive_name = staged
            .archive
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ctx = adjust::Mod {
            name: &name,
            archive: &archive_name,
            files: &pre,
            stop_folders: &self.game.stop_folders,
            content_name,
        };
        let special = crate::special::capture(&self.game, &ctx, &|d| fs::read(src_of(d)?).ok());
        for (src, dest, game_root) in raw {
            match game_root {
                true => insert(src, dest, true),
                false => match adjust::adjust(profile, &dest, &ctx) {
                    Some(Dest::Install(d)) => insert(src, d, false),
                    Some(Dest::GameRoot(d)) => {
                        profile_root = true;
                        insert(src, d, true)
                    }
                    None => {}
                },
            }
        }

        let (mut files, mut root_files) = (vec![], vec![]);
        let n = map.len() as u64;
        self.mark_pending(&staged.id)?;
        if let Some(p) = prev.as_ref().filter(|_| !same) {
            self.mark_pending(&p.id)?;
        }
        let copied = map
            .into_values()
            .enumerate()
            .try_for_each(|(i, (src, dest, game_root))| {
                self.task.step("Copying mod files", i as u64, n)?;
                let to = self
                    .place(&Deployed {
                        path: dest.clone(),
                        mod_id: staged.id.clone(),
                        game_root,
                        src: None,
                    })
                    .src;
                fs::create_dir_all(to.parent().unwrap()).at(to.parent().unwrap())?;
                fs::copy(&src, &to).at(&to)?;
                if game_root {
                    &mut root_files
                } else {
                    &mut files
                }
                .push(dest);
                Ok::<_, Error>(())
            });
        let (readmes, screenshot) = match copied {
            Ok(()) => (self.keep_readmes(&staged), self.keep_screenshot(&staged)),
            Err(_) => (vec![], None),
        };
        let _ = fs::remove_dir_all(&staged.dir);
        if let Err(e) = copied {
            if let Err(e) = self.resolve_pending() {
                tracing::warn!("cleanup after failed install failed: {e}");
            }
            return Err(e);
        }
        if let (Some((_, cfg)), Some(sel)) = (&staged.fomod, sel) {
            let choices = cfg.choices(sel);
            self.state.fomod_choices.insert(staged.id.clone(), choices);
        }
        let entry = ModEntry {
            id: staged.id,
            name,
            archive: staged.archive.to_string_lossy().into_owned(),
            files,
            root_files,
            game_root: match &prev {
                Some(p) if !p.root_files.is_empty() => p.game_root,
                _ => game_root || profile_root,
            },
            active: prev.as_ref().is_none_or(|p| p.active),
            nexus: staged
                .nexus
                .or_else(|| prev.as_ref().filter(|_| same)?.nexus.clone()),
            special,
            readmes,
            info: staged.info,
            screenshot,
        };
        // C# `IniInstaller`: the script's INI edits, undone when the mod goes.
        let entry_id = entry.id.clone();
        let record_ini = |m: &mut Self| {
            for (file, section, key, value) in script_plan.iter().flat_map(|p| &p.ini) {
                m.edit_ini(&entry_id, file, section, key, value);
            }
        };
        let (Some(i), Some(prev)) = (slot, prev) else {
            self.state.mods.push(entry);
            record_ini(self);
            self.deploy()?;
            if let Some(p) = &script_plan {
                self.apply_script_plugins(p);
            }
            return self.resolve_pending();
        };
        for ids in self.state.profiles.values_mut() {
            ids.iter_mut()
                .filter(|id| **id == prev.id)
                .for_each(|id| *id = entry.id.clone());
        }
        self.state.mods[i] = entry;
        // A reinstall rewrote the stored files under the same paths: relink them (copies are stale).
        self.deploy_with(same.then_some(&prev.id[..]))?;
        if same {
            let keep: HashSet<String> = self.state.mods[i]
                .deployable(true)
                .map(|(f, r)| key(f, r))
                .collect();
            for (f, r) in prev
                .deployable(true)
                .filter(|(f, r)| !keep.contains(&key(f, *r)))
            {
                let store = match r {
                    true => self.mods_dir.join("GameRootInstall").join(&prev.id),
                    false => self.virtual_dir(&prev.id),
                };
                let _ = fs::remove_file(store.join(f));
                prune(store.join(f).parent().unwrap(), &store);
            }
        }
        // C# `IniUpgradeInstaller.FinalizeInstall` undoes the old version's INI edits the new one
        // doesn't make again.
        for e in &mut self.state.ini_edits {
            e.by.retain(|(id, _)| *id != prev.id);
        }
        record_ini(self);
        self.finish(&HashSet::new())?;
        if let Some(p) = &script_plan {
            self.apply_script_plugins(p);
        }
        self.resolve_pending()
    }

    /// Run an intent change (+ its deploy) all-or-nothing w.r.t. cancel: if `op` is cancelled,
    /// restore the previous mod list and link mode, deploy back to them (not cancellable), and
    /// delete files of mods `op` was installing, and put the load order back. Still returns
    /// `Err(Cancelled)`.
    pub fn transact(&mut self, op: impl FnOnce(&mut Self) -> R<()>) -> R<()> {
        let (mods, link_mode) = (self.state.mods.clone(), self.state.link_mode);
        let profiles = self.state.profiles.clone();
        let plugins = plugins::Policy::new(&self.game, &self.game_path).map(|p| p.read());
        let result = op(self);
        if !matches!(result, Err(Error::Cancelled)) {
            return result;
        }
        tracing::info!("cancelled; rolling back");
        if link_mode != self.state.link_mode {
            // Part of the deploy was relinked in the new mode; relink everything back.
            self.state.deployed_mode = self.state.link_mode;
        }
        self.state.mods = mods;
        self.state.profiles = profiles;
        self.state.link_mode = link_mode;
        let progress = self.task.progress.clone();
        let task = std::mem::replace(
            &mut self.task,
            Task {
                cancel: Default::default(),
                progress: Arc::new(move |_, done, total| progress("Rolling back", done, total)),
            },
        );
        let rolled_back = self.deploy();
        self.task = task;
        rolled_back.inspect_err(|e| tracing::error!("rollback failed: {e}"))?;
        // The deploy re-added plugins `op` had unlinked as fresh (last, active); restore them.
        if let Some(Err(e)) = plugins.map(|p| self.set_plugins(&p)) {
            tracing::warn!("load order not restored after rollback: {e}");
        }
        tracing::info!("rolled back");
        // Files of a mod the op added (or was removing) are kept or deleted per the restored list.
        self.resolve_pending()?;
        result
    }

    /// Files in the plugin folder that are neither base game (`baseFilesPath`), deployed by this
    /// manager, official Creation content, nor the load order files: C# `CheckLooseFiles`. None
    /// for games without a base-file list, where the game's own files can't be told apart.
    pub fn loose_files(&self) -> Option<Vec<String>> {
        let base = self.game.base_files();
        if base.is_empty() {
            return None;
        }
        let root = self
            .game
            .plugin_dir(&self.game_path)
            .unwrap_or_else(|| self.install_root());
        let mut keep: HashSet<String> = base.iter().map(|b| b.to_lowercase()).collect();
        let policy = plugins::Policy::new(&self.game, &self.game_path);
        let mut stems: HashSet<String> = HashSet::new();
        if let Some(p) = &policy {
            for f in [&p.plugins_file, &p.load_order_file] {
                if let Ok(r) = f.strip_prefix(&root) {
                    keep.insert(r.to_string_lossy().replace('\\', "/").to_lowercase());
                }
            }
            // Creation Club plugins (.ccc) and their archives (`ccXYZ - Textures.bsa`).
            for o in &p.official {
                let l = o.to_lowercase();
                stems.insert(l.rsplit_once('.').map_or(l.clone(), |(s, _)| s.to_string()));
                keep.insert(l);
            }
        }
        let deployed: HashSet<&str> = self
            .state
            .deployed
            .iter()
            .filter(|(_, d)| !d.game_root)
            .map(|(k, _)| k.as_str())
            .collect();
        let install_root = self.install_root();
        let mut out: Vec<String> = files_under(&root)
            .iter()
            .filter_map(|f| {
                let r = rel(f, &root);
                let l = r.to_lowercase();
                let deployed_key = f
                    .strip_prefix(&install_root)
                    .map(|x| x.to_string_lossy().replace('\\', "/").to_lowercase());
                let ours = deployed_key.is_ok_and(|k| deployed.contains(k.as_str()));
                let creation = !l.contains('/')
                    && stems
                        .iter()
                        .any(|s| l.starts_with(s.as_str()) && l[s.len()..].starts_with([' ', '.']));
                (!keep.contains(&l) && !ours && !creation).then_some(r)
            })
            .collect();
        out.sort_by_key(|f| f.to_lowercase());
        Some(out)
    }

    /// Move `loose_files` out of the game into `<mods>/LooseFiles/<unix time>/`, where they can be
    /// put back by hand. Base game, deployed and Creation files are never touched. Returns the folder.
    pub fn purge_loose(&self) -> R<Option<PathBuf>> {
        let Some(files) = self.loose_files().filter(|f| !f.is_empty()) else {
            return Ok(None);
        };
        let root = self
            .game
            .plugin_dir(&self.game_path)
            .unwrap_or_else(|| self.install_root());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let dest = self.mods_dir.join("LooseFiles").join(now.to_string());
        let n = files.len() as u64;
        for (i, f) in files.iter().enumerate() {
            self.task.step("Moving loose files", i as u64, n)?;
            let (from, to) = (root.join(f), dest.join(f));
            fs::create_dir_all(to.parent().unwrap()).at(to.parent().unwrap())?;
            move_file(&from, &to)?;
            prune(from.parent().unwrap(), &root);
        }
        tracing::info!(game = %self.game.mode_id, files = n, to = %dest.display(), "purged loose files");
        Ok(Some(dest))
    }

    /// Record mod `id` setting `key` in `section` of INI `file` (C# `IniInstaller.EditIni`, for
    /// script installers). Deploy applies it while the mod is active and puts back the previous
    /// editor's value, else the original, once it isn't (or is removed).
    pub fn edit_ini(&mut self, id: &str, file: &Path, section: &str, key: &str, value: &str) {
        crate::ini::record(&mut self.state.ini_edits, id, file, section, key, value);
    }

    /// The game INI (`None`) or another settings file by C# key (`FOPrefsIniPath`), inside the
    /// Wine/Proton prefix when the game keeps its settings there.
    pub fn settings_file(&self, key: Option<&str>) -> Option<PathBuf> {
        let g = self.game.gamebryo.as_ref()?;
        let t = match key {
            None => g.ini_file_path.as_ref()?,
            Some(k) => g.additional_settings_files.get(k)?,
        };
        Some(self.game.settings_path(t, &self.game_path))
    }

    /// A question to ask before removing `id` (State of Decay / X Rebirth save-recorded mods).
    pub fn removal_warning(&self, id: &str) -> Option<String> {
        crate::special::removal_warning(self.state.mods.iter().find(|m| m.id == id)?)
    }

    pub fn remove(&mut self, id: &str) -> R<()> {
        self.mark_pending(id)?;
        self.state.mods.retain(|m| m.id != id);
        self.deploy()?;
        self.resolve_pending()
    }

    pub fn set_active(&mut self, id: &str, active: bool) -> R<()> {
        self.state
            .mods
            .iter_mut()
            .filter(|m| m.id == id)
            .for_each(|m| m.active = active);
        self.deploy()
    }

    pub fn set_link_mode(&mut self, mode: LinkMode) -> R<()> {
        self.state.link_mode = mode;
        self.deploy()
    }

    /// Whether the mods folder and the game share a volume, i.e. hardlinks can work.
    pub fn same_volume(&self) -> bool {
        let (a, b) = (
            fs::canonicalize(&self.mods_dir),
            fs::canonicalize(&self.game_path),
        );
        let (Ok(a), Ok(b)) = (a, b) else { return false };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            matches!((fs::metadata(a), fs::metadata(b)), (Ok(x), Ok(y)) if x.dev() == y.dev())
        }
        // ponytail: drive-letter / UNC-share compare; mounted folders inside a drive read as the same volume.
        #[cfg(windows)]
        {
            let prefix = |p: &Path| {
                p.components()
                    .next()
                    .map(|c| c.as_os_str().to_ascii_lowercase())
            };
            prefix(&a) == prefix(&b)
        }
    }

    pub fn set_game_root(&mut self, id: &str, on: bool) -> R<()> {
        self.state
            .mods
            .iter_mut()
            .filter(|m| m.id == id)
            .for_each(|m| m.game_root = on);
        self.deploy()
    }

    /// Move a mod `delta` places in load order (positive = later = higher priority).
    pub fn reorder(&mut self, id: &str, delta: isize) -> R<()> {
        let Some(i) = self.state.mods.iter().position(|m| m.id == id) else {
            return Ok(());
        };
        let j = (i as isize + delta).clamp(0, self.state.mods.len() as isize - 1) as usize;
        let m = self.state.mods.remove(i);
        self.state.mods.insert(j, m);
        self.deploy()
    }

    // ---------- profiles ----------

    pub fn save_profile(&mut self, name: &str) -> R<()> {
        let active = self
            .state
            .mods
            .iter()
            .filter(|m| m.active)
            .map(|m| m.id.clone())
            .collect();
        self.state.profiles.insert(name.into(), active);
        match plugins::Policy::new(&self.game, &self.game_path) {
            Some(policy) => self
                .state
                .profile_plugins
                .insert(name.into(), policy.read()),
            None => self.state.profile_plugins.remove(name),
        };
        self.save()
    }

    /// Switch active mods, then the saved plugin order and active states (C#
    /// `ApplyPendingProfileLoadOrder`): saved plugins now missing are skipped, plugins the profile
    /// doesn't list go inactive. A plugin state that would break the load order is refused and the
    /// whole switch rolled back.
    pub fn load_profile(&mut self, name: &str) -> R<()> {
        let ids = self
            .state
            .profiles
            .get(name)
            .cloned()
            .ok_or("no such profile")?;
        let before: Vec<bool> = self.state.mods.iter().map(|m| m.active).collect();
        let policy = plugins::Policy::new(&self.game, &self.game_path);
        let order_before = policy.as_ref().map(|p| p.read());
        for m in &mut self.state.mods {
            m.active = ids.contains(&m.id);
        }
        self.deploy()?;
        let Some(saved) = self.state.profile_plugins.get(name) else {
            return Ok(());
        };
        let Err(e) = self.replace_plugins(saved) else {
            return Ok(());
        };
        tracing::warn!(name, "profile plugin state refused, rolling back: {e}");
        for (m, a) in self.state.mods.iter_mut().zip(before) {
            m.active = a;
        }
        self.deploy()?;
        if let Some(order) = order_before {
            // ponytail: best effort; it was the live state a moment ago.
            if let Err(e) = self.replace_plugins(&order) {
                tracing::warn!("restoring the previous plugin order failed: {e}");
            }
        }
        Err(format!("Profile \"{name}\" not loaded: {e}").into())
    }

    /// `set_plugins` with `saved` as the whole state: unlisted plugins go inactive.
    fn replace_plugins(&self, saved: &[plugins::Plugin]) -> R<Vec<PluginRow>> {
        let mut wanted = saved.to_vec();
        if let Some(policy) = plugins::Policy::new(&self.game, &self.game_path) {
            for c in policy.read() {
                if !saved.iter().any(|s| s.name.eq_ignore_ascii_case(&c.name)) {
                    wanted.push(plugins::Plugin {
                        name: c.name,
                        active: false,
                    });
                }
            }
        }
        self.set_plugins(&wanted)
    }

    pub fn delete_profile(&mut self, name: &str) -> R<()> {
        self.state.profiles.remove(name);
        self.state.profile_plugins.remove(name);
        self.save()
    }

    /// Every (key, link) active mods want, in load order; later entries win a key.
    fn wanted(&self) -> Vec<(String, Deployed)> {
        self.links(false)
    }

    /// Links of active mods, or of every mod and file (`all`), in load order.
    fn links(&self, all: bool) -> Vec<(String, Deployed)> {
        let mut out = vec![];
        for (i, m) in self
            .state
            .mods
            .iter()
            .enumerate()
            .filter(|(_, m)| all || m.active)
        {
            for (f, game_root) in m.deployable(all) {
                let path = adjust::resolve(f, i);
                let src = (path != *f).then(|| f.clone());
                out.push((
                    key(&path, game_root),
                    Deployed {
                        path,
                        mod_id: m.id.clone(),
                        game_root,
                        src,
                    },
                ));
            }
        }
        out
    }

    /// Which mods each mod overrides / is overridden by, for the UI.
    pub fn conflicts(&self) -> BTreeMap<String, Vec<String>> {
        let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, d) in self.wanted() {
            owners.entry(k).or_default().push(d.mod_id);
        }
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for ids in owners.values().filter(|v| v.len() > 1) {
            for id in ids {
                let e = out.entry(id.to_string()).or_default();
                for other in ids.iter().filter(|o| o != &id) {
                    if !e.iter().any(|x| x == other) {
                        e.push(other.to_string());
                    }
                }
            }
        }
        out
    }

    // ---------- deploy ----------

    /// Whether what's linked matches what active mods want; false after a cancelled or failed deploy.
    pub fn in_sync(&self) -> bool {
        let desired: BTreeMap<String, Deployed> = self.wanted().into_iter().collect();
        self.state.link_mode == self.state.deployed_mode && desired == self.state.deployed
    }

    fn journal_path(&self) -> PathBuf {
        self.mods_dir.join("deploy.journal")
    }

    pub fn deploy(&mut self) -> R<()> {
        self.deploy_with(None)
    }

    /// `deploy`, also relinking every path mod `refresh` deploys.
    fn deploy_with(&mut self, refresh: Option<&str>) -> R<()> {
        let desired: BTreeMap<String, Deployed> = self.wanted().into_iter().collect();
        let deployed = &self.state.deployed;
        let relink = self.state.link_mode != self.state.deployed_mode;
        let journal: Journal = deployed
            .keys()
            .chain(desired.keys())
            .filter(|k| {
                relink
                    || deployed.get(*k) != desired.get(*k)
                    || desired
                        .get(*k)
                        .is_some_and(|d| Some(&d.mod_id[..]) == refresh)
            })
            .map(|k| {
                (
                    k.clone(),
                    (deployed.get(k).cloned(), desired.get(k).cloned()),
                )
            })
            .collect();

        // ponytail: link ops themselves aren't flushed; relies on APFS/NTFS/ext4 ordering metadata ops.
        if !journal.is_empty() {
            write_durable(
                &self.journal_path(),
                &serde_json::to_vec(&journal).map_err(|e| e.to_string())?,
            )?;
        }
        if !journal.is_empty() {
            // Best effort, as in C#: a mod still deploys when the INI can't be fixed.
            if let Some(Err(e)) =
                crate::invalidation::starfield_loose_files(&self.game, &self.game_path)
            {
                tracing::warn!("StarfieldCustom.ini could not be configured for loose files: {e}");
            }
        }
        let result = self.apply(&journal);
        match &result {
            Ok(()) if !journal.is_empty() => {
                tracing::info!(changes = journal.len(), mode = ?self.state.link_mode, game = %self.game.mode_id, "deployed")
            }
            Err(Error::Cancelled) | Ok(()) => {}
            Err(e) => tracing::error!(game = %self.game.mode_id, "deploy failed, reconciling: {e}"),
        }
        if result.is_ok() {
            self.state.deployed_mode = self.state.link_mode;
        }
        if !journal.is_empty()
            && self.game.mod_install.path_adjustment_profile == adjust::Profile::NoMansSky
        {
            // NMS caches its mod list here; stale entries hide changed mods. Best effort, as in C#.
            let _ = fs::remove_file(self.game_path.join("Binaries/SETTINGS/GCMODSETTINGS.MXML"));
        }
        if result.is_err() {
            self.reconcile(&candidates(&journal))?;
        }
        let added = journal
            .iter()
            .filter(|(_, (old, new))| old.is_none() && new.as_ref().is_some_and(|d| !d.game_root))
            .map(|(k, _)| k.clone())
            .collect();
        self.finish(&added)?;
        result
    }

    /// Persist state after a deploy or recovery; only then is the journal obsolete.
    fn finish(&mut self, added: &HashSet<String>) -> R<()> {
        self.sync_plugins(added)?;
        let mut applied = std::mem::take(&mut self.state.special_applied);
        let store = |id: &str, f: &str| self.virtual_dir(id).join(f);
        crate::special::sync(
            &self.game,
            &self.game_path,
            &self.state.mods,
            &store,
            &mut applied,
        );
        self.state.special_applied = applied;
        let mods = &self.state.mods;
        crate::ini::sync_edits(&mut self.state.ini_edits, |id| {
            mods.iter().find(|m| m.id == id).map(|m| m.active)
        });
        self.save()?;
        match fs::remove_file(self.journal_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(Error::Io {
                path: self.journal_path(),
                source: e,
            }),
            _ => Ok(()),
        }
    }

    fn apply(&mut self, journal: &Journal) -> R<()> {
        // A cancel is just an early error: deploy() reconciles the partial apply like any failure.
        let n = journal
            .values()
            .map(|(o, n)| o.is_some() as u64 + n.is_some() as u64)
            .sum();
        let mut i = 0;
        let mut step = |task: &Task| {
            i += 1;
            task.step("Deploying", i - 1, n)
        };
        for (key, (old, new)) in journal {
            let Some(d) = old else { continue };
            step(&self.task)?;
            let Place {
                target,
                saved,
                root,
                backup,
                ..
            } = self.place(d);
            match fs::remove_file(&target) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(Error::Io {
                        path: target,
                        source: e,
                    })
                }
            }
            self.state.deployed.remove(key);
            crash_point();
            if new.is_none() && saved.exists() {
                move_file(&saved, &target)?;
                crash_point();
                prune(saved.parent().unwrap(), &backup);
            } else {
                prune(target.parent().unwrap(), &root);
            }
        }
        for (key, (_, new)) in journal {
            let Some(d) = new else { continue };
            step(&self.task)?;
            let Place {
                target, saved, src, ..
            } = self.place(d);
            if target.exists() && !saved.exists() {
                move_file(&target, &saved)?;
                crash_point();
            } else if target.exists() {
                let _ = fs::remove_file(&target);
            }
            let g = &self.game;
            place_file(
                &src,
                &target,
                self.state.link_mode,
                g.needs_real_copy(&d.path),
                g.needs_hardlink(&d.path),
            )?;
            crash_point();
            self.state.deployed.insert(key.clone(), d.clone());
        }
        Ok(())
    }

    /// Rebuild `deployed` for the given paths from what is actually on disk, and put back
    /// game files stranded in Backup. Converges any partial `apply`; the next deploy redoes the rest.
    fn reconcile(&mut self, cands: &Candidates) -> R<()> {
        for (key, (at, links)) in cands {
            let Place {
                target,
                saved,
                backup,
                ..
            } = self.place(at);
            for t in [&target, &saved] {
                let _ = fs::remove_file(tmp_path(t));
            }
            let live = links.iter().find(|d| {
                let p = self.place(d);
                same_content(&p.src, &p.target)
            });
            if let Some(d) = live {
                self.state.deployed.insert(key.clone(), d.clone());
                continue;
            }
            self.state.deployed.remove(key);
            if !saved.exists() {
                continue;
            }
            if !target.exists() {
                move_file(&saved, &target)?;
                prune(saved.parent().unwrap(), &backup);
            } else if same_content(&saved, &target) {
                // Crash inside move_file's copy fallback left two copies.
                fs::remove_file(&saved).at(&saved)?;
                prune(saved.parent().unwrap(), &backup);
            } else {
                // Not ours, not the backup: the game replaced the file (an update). Keep the live one,
                // set the stale backup aside, or the next deploy would overwrite the live one with it.
                // ponytail: a later orphan of the same path replaces an earlier one.
                let aside = self
                    .mods_dir
                    .join("OrphanedBackup")
                    .join(rel(&saved, &self.mods_dir));
                move_file(&saved, &aside)?;
                prune(saved.parent().unwrap(), &backup);
                tracing::warn!(path = %aside.display(), "stale backup set aside");
            }
        }
        Ok(())
    }

    /// Mod folders in the store with no entry in the mod list, e.g. after `state.json` was lost.
    pub fn strays(&self) -> Vec<String> {
        let mut ids = std::collections::BTreeSet::new();
        for dir in ["VirtualInstall", "GameRootInstall"] {
            let entries = fs::read_dir(self.mods_dir.join(dir))
                .into_iter()
                .flatten()
                .filter_map(Result::ok);
            ids.extend(
                entries
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().into_owned()),
            );
        }
        ids.retain(|id| {
            !self.state.mods.iter().any(|m| &m.id == id)
                && !self.mods_dir.join(".pending").join(id).exists()
        });
        ids.into_iter().collect()
    }

    /// Re-add every stray mod folder, appended in name order, then scan the game: mods whose
    /// files are already live come back enabled (and are completed), the rest stay off.
    // ponytail: name = folder id, no archive; the original names lived only in state.json.
    pub fn adopt(&mut self) -> R<RepairReport> {
        let ids = self.strays();
        let list = |dir: PathBuf| {
            let mut v: Vec<_> = files_under(&dir).iter().map(|f| rel(f, &dir)).collect();
            v.sort_by_key(|f| f.to_lowercase()); // install's order
            v
        };
        for id in &ids {
            let (files, root_files) = (
                list(self.virtual_dir(id)),
                list(self.mods_dir.join("GameRootInstall").join(id)),
            );
            self.state.mods.push(ModEntry {
                id: id.clone(),
                name: id.clone(),
                archive: String::new(),
                files,
                root_files,
                game_root: false,
                active: false,
                nexus: None,
                special: BTreeMap::new(),
                readmes: list(self.readme_dir(id)),
                info: None,
                screenshot: list(self.screenshot_dir(id)).into_iter().next(),
            });
        }
        let report = self.scan()?;
        let live: Vec<(String, bool)> = self
            .state
            .deployed
            .values()
            .map(|d| (d.mod_id.clone(), d.game_root))
            .collect();
        for m in self.state.mods.iter_mut().filter(|m| ids.contains(&m.id)) {
            m.active = live.iter().any(|(id, _)| *id == m.id);
            m.game_root = live.iter().any(|(id, root)| *id == m.id && *root);
        }
        tracing::info!(?ids, "re-added stray mods");
        self.finish(&HashSet::new())?;
        self.deploy()?;
        Ok(report)
    }

    /// Last resort for a lost or unreadable `deploy.journal`: `reconcile` every path any mod,
    /// `deployed` or a backup knows, then redeploy the saved intent.
    pub fn repair(&mut self) -> R<RepairReport> {
        let report = self.scan()?;
        self.finish(&HashSet::new())?;
        self.deploy()?;
        Ok(report)
    }

    fn scan(&mut self) -> R<RepairReport> {
        // A path nothing recorded and nothing backed up may be a game file that happens to equal
        // a mod's: count it as deployed only when it's provably a link, or its mod provably has
        // other files live (copy-only files like .exe can't be proven links), never by content alone.
        let mut trusted: std::collections::HashSet<String> =
            self.state.deployed.keys().cloned().collect();
        let backups = self.backups();
        trusted.extend(backups.iter().map(|(g, p)| key(p, *g)));
        let all = self.links(true);
        let links: Vec<_> = (self
            .wanted()
            .into_iter()
            .rev()
            .chain(self.state.deployed.clone())
            .chain(all.into_iter().rev()))
        .map(|(k, d)| {
            let p = self.place(&d);
            let linked = same_file(&p.src, &p.target);
            (k, d, linked)
        })
        .collect();
        let mut proven: std::collections::HashSet<String> = self
            .state
            .deployed
            .values()
            .map(|d| d.mod_id.clone())
            .collect();
        proven.extend(
            links
                .iter()
                .filter(|(_, _, linked)| *linked)
                .map(|(_, d, _)| d.mod_id.clone()),
        );
        let links: Vec<_> = links
            .into_iter()
            .map(|(k, d, linked)| {
                let live = linked || trusted.contains(&k) || proven.contains(&d.mod_id);
                (k, d, live)
            })
            .collect();
        let mut cands = Candidates::new();
        let mut add = |k: String, d: Deployed, live: bool| {
            let (_, links) = cands.entry(k).or_insert_with(|| (d.clone(), vec![]));
            if live && !links.contains(&d) {
                links.push(d);
            }
        };
        // Likeliest first: what should win now, what we last recorded, then any mod's copy.
        let mut untrusted = HashMap::new();
        for (k, d, live) in links {
            if !live {
                untrusted.entry(k.clone()).or_insert_with(|| d.clone());
            }
            add(k, d, live);
        }
        for (game_root, path) in backups {
            add(
                key(&path, game_root),
                Deployed {
                    path,
                    mod_id: String::new(),
                    game_root,
                    src: None,
                },
                false,
            );
        }
        tracing::info!(paths = cands.len(), "scanning game against mods");
        let aside = |m: &Self| {
            files_under(&m.mods_dir.join("OrphanedBackup"))
                .iter()
                .map(|f| rel(f, &m.mods_dir))
                .collect::<Vec<_>>()
        };
        let before = aside(self);
        self.reconcile(&cands)?;
        let orphaned_backups = aside(self)
            .into_iter()
            .filter(|f| !before.contains(f))
            .collect();
        let untracked = cands
            .iter()
            .filter(|(k, (_, live))| live.is_empty() && !self.state.deployed.contains_key(*k))
            .filter_map(|(k, _)| untrusted.get(k))
            .filter(|d| {
                let p = self.place(d);
                same_content(&p.src, &p.target)
            })
            .map(|d| {
                // Outside the game folder for managed / secondary install roots: show it whole.
                let t = self.place(d).target;
                match t.strip_prefix(&self.game_path) {
                    Ok(r) => r.to_string_lossy().replace('\\', "/"),
                    Err(_) => t.to_string_lossy().into_owned(),
                }
            })
            .collect();
        let report = RepairReport {
            orphaned_backups,
            untracked,
        };
        if report != RepairReport::default() {
            tracing::warn!(?report, "repair left files for review");
        }
        Ok(report)
    }

    /// (is game-root, path) of every backed-up game file. Deletes copy scratch files on the way:
    /// a backup copy in flight always still has its source.
    fn backups(&self) -> Vec<(bool, String)> {
        let mut out = vec![];
        for (game_root, dir) in [(false, "Backup"), (true, "GameRootBackup")] {
            let base = self.mods_dir.join(dir);
            for e in walkdir::WalkDir::new(&base)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_file())
            {
                let path = rel(e.path(), &base);
                if path.ends_with(".nmm-tmp") {
                    let _ = fs::remove_file(e.path());
                } else {
                    out.push((game_root, path));
                }
            }
        }
        out
    }

    // ---------- plugins ----------

    fn plugin_rows(
        &self,
        policy: &plugins::Policy,
        entries: Vec<plugins::Entry>,
    ) -> Vec<PluginRow> {
        entries
            .into_iter()
            .map(|e| PluginRow {
                locked_order: policy.is_fixed(&e.name),
                locked_active: policy.is_forced_active(&e.name),
                owner: self
                    .state
                    .deployed
                    .get(&key(&e.name, false))
                    .and_then(|d| self.state.mods.iter().find(|m| m.id == d.mod_id))
                    .map(|m| m.name.clone()),
                entry: e,
            })
            .collect()
    }

    /// The game's plugins in load order; None when it doesn't use plugins.
    pub fn plugins(&self) -> Option<Vec<PluginRow>> {
        let policy = plugins::Policy::new(&self.game, &self.game_path)?;
        let list = policy.read();
        let headers = policy.headers(&list);
        Some(self.plugin_rows(&policy, policy.check(&list, &headers)))
    }

    /// Set plugin order and active states (C# `TryApplyPluginState`). The ordering rules are
    /// re-applied (critical plugins on top, masters above dependents); a change that adds an
    /// error (activating a plugin whose master is off, deactivating a master in use, running
    /// out of slots) is refused whole. Plugins left out of `wanted` keep their place at the end.
    pub fn set_plugins(&self, wanted: &[plugins::Plugin]) -> R<Vec<PluginRow>> {
        let policy = plugins::Policy::new(&self.game, &self.game_path)
            .ok_or("this game doesn't use plugins")?;
        let current = policy.read();
        let headers = policy.headers(&current);
        let errors = |entries: &[plugins::Entry]| -> Vec<String> {
            entries
                .iter()
                .flat_map(|e| {
                    e.diagnostics
                        .iter()
                        .filter(|d| d.error)
                        .map(move |d| format!("{}: {}", e.name, d.message))
                })
                .collect()
        };
        let before = errors(&policy.check(&current, &headers));
        let mut list: Vec<plugins::Plugin> = wanted
            .iter()
            .filter_map(|w| {
                let c = current
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(&w.name))?;
                Some(plugins::Plugin {
                    name: c.name.clone(),
                    active: w.active,
                })
            })
            .collect();
        for c in &current {
            if !list.iter().any(|p| p.name == c.name) {
                list.push(c.clone());
            }
        }
        policy.correct(&mut list, &headers);
        let entries = policy.check(&list, &headers);
        let new: Vec<String> = errors(&entries)
            .into_iter()
            .filter(|e| !before.contains(e))
            .collect();
        if !new.is_empty() {
            return Err(new.join("; ").into());
        }
        policy.write(&list)?;
        Ok(self.plugin_rows(&policy, entries))
    }

    /// LOOT's proposed order next to the current one, for a preview before applying it with
    /// `set_plugins`. Refreshes the masterlist first when it's missing or a day old; if that
    /// fails the sort goes on with the cached one (or masters only) and `note` says so.
    pub fn loot_preview(&self) -> R<LootPreview> {
        let policy = plugins::Policy::new(&self.game, &self.game_path)
            .ok_or("this game doesn't use plugins")?;
        let mode = &self.game.mode_id;
        let dir = self.mods_dir.join("loot");
        let age = loot::masterlist_age(&dir);
        let mut note = None;
        if age.is_none_or(|a| a > loot::MAX_AGE) {
            if let Err(e) = loot::update_masterlist(mode, &dir) {
                tracing::warn!("LOOT masterlist update failed: {e}");
                note = Some(format!(
                    "Couldn't update the LOOT masterlist ({e}); {}.",
                    if age.is_some() {
                        "used the saved one"
                    } else {
                        "sorted by masters only"
                    }
                ));
            }
        }
        let list = policy.read();
        let current: Vec<String> = list.iter().map(|p| p.name.clone()).collect();
        let order = loot::sort(mode, &policy, &self.game_path, &dir, &current)?;
        // Show what applying gives: the game's own rules still win over LOOT.
        let mut sorted: Vec<plugins::Plugin> = order
            .into_iter()
            .map(|name| plugins::Plugin {
                name,
                active: false,
            })
            .collect();
        policy.correct(&mut sorted, &policy.headers(&list));
        Ok(LootPreview {
            current,
            sorted: sorted.into_iter().map(|p| p.name).collect(),
            note,
        })
    }

    /// Use `src` as this game's LOOT userlist (the user's own sorting rules) from now on.
    pub fn loot_import_userlist(&self, src: &Path) -> R<()> {
        let policy = plugins::Policy::new(&self.game, &self.game_path)
            .ok_or("this game doesn't use plugins")?;
        loot::import_userlist(
            &self.game.mode_id,
            &policy,
            &self.game_path,
            &self.mods_dir.join("loot"),
            src,
        )
    }

    /// Plugin order + active states as text (C# `PluginManagerVM.ExportLoadOrder`):
    /// `GameMode=<mode>`, then `<plugin>=1|0` per line.
    pub fn export_load_order(&self) -> R<String> {
        let policy = plugins::Policy::new(&self.game, &self.game_path)
            .ok_or("this game doesn't use plugins")?;
        let mut out = format!("GameMode={}\r\n", self.game.mode_id);
        for p in policy.read() {
            out += &format!("{}={}\r\n", p.name, u8::from(p.active));
        }
        Ok(out)
    }

    /// Apply an exported load order (C# `ImportLoadOrder`): listed plugins take its order, and
    /// its active state where given (`name` alone only moves); unlisted plugins keep their state.
    /// Applied through `set_plugins`, so the game's rules hold and breaking changes are refused.
    pub fn import_load_order(&self, text: &str) -> R<ImportReport> {
        let mut mode = None;
        let mut listed: Vec<(String, Option<bool>)> = vec![];
        for line in text.lines().map(str::trim) {
            if let Some(m) = line.strip_prefix("GameMode=") {
                if mode.replace(m).is_some() {
                    return Err("the load order has more than one game mode".into());
                }
                continue;
            }
            let (name, active) = match line.rsplit_once('=') {
                Some((n, "1")) => (n, Some(true)),
                Some((n, "0")) => (n, Some(false)),
                _ => (line, None),
            };
            let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
            if matches!(ext.as_deref(), Some("esp" | "esm" | "esl"))
                && !listed.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
            {
                listed.push((name.to_string(), active));
            }
        }
        match mode {
            None => return Err("the load order has no game mode".into()),
            Some(m) if m != self.game.mode_id => {
                return Err(format!("the load order is for {m}, not {}", self.game.mode_id).into())
            }
            _ => {}
        }
        if listed.is_empty() {
            return Err("No plugins were found in the load order.".into());
        }
        let policy = plugins::Policy::new(&self.game, &self.game_path)
            .ok_or("this game doesn't use plugins")?;
        let current = policy.read();
        let mut wanted = vec![];
        let mut unknown = vec![];
        for (name, active) in &listed {
            match current.iter().find(|c| c.name.eq_ignore_ascii_case(name)) {
                Some(c) => wanted.push(plugins::Plugin {
                    name: c.name.clone(),
                    active: active.unwrap_or(c.active),
                }),
                None => unknown.push(name.clone()),
            }
        }
        if wanted.is_empty() {
            return Err("None of the plugins in the load order are in the game.".into());
        }
        self.set_plugins(&wanted)?;
        Ok(ImportReport {
            total: listed.len(),
            imported: wanted.len(),
            unknown,
        })
    }

    /// Persist the plugin load order after a deploy. `added` are paths this deploy linked fresh:
    /// their plugins load last and start active (C# `IntegrateDeployedPlugins`); everything else
    /// keeps the order and state the game's files already hold.
    fn sync_plugins(&self, added: &HashSet<String>) -> R<()> {
        let Some(policy) = plugins::Policy::new(&self.game, &self.game_path) else {
            return Ok(());
        };
        let mut list = policy.read();
        let mut fresh = vec![];
        for m in &self.state.mods {
            for f in m.files.iter().filter(|f| added.contains(&key(f, false))) {
                if let Some(i) = list.iter().position(|p| p.name.eq_ignore_ascii_case(f)) {
                    fresh.push(list[i].name.clone());
                    let p = list.remove(i);
                    list.push(p);
                }
            }
        }
        let headers = policy.headers(&list);
        policy.correct(&mut list, &headers);
        // Activate each fresh plugin unless that gives it an error (missing master, no free slot);
        // repeat so a fresh master activated later lets its dependents in.
        let mut pending: Vec<String> = list
            .iter()
            .filter(|p| fresh.contains(&p.name) && !p.active)
            .map(|p| p.name.clone())
            .collect();
        loop {
            let before = pending.len();
            pending.retain(|name| {
                let i = list.iter().position(|p| &p.name == name).unwrap();
                list[i].active = true;
                if policy.check(&list, &headers)[i].has_errors() {
                    list[i].active = false;
                    return true;
                }
                false
            });
            if pending.len() == before {
                break;
            }
        }
        if !pending.is_empty() {
            tracing::warn!(
                ?pending,
                "deployed plugins left inactive: activating them would break the load order"
            );
        }
        policy.write(&list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ini;
    use std::io::Write;
    use std::sync::Arc;

    fn zip(path: &Path, files: &[(&str, &str)]) {
        let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
        for (n, body) in files {
            z.start_file(*n, zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(body.as_bytes()).unwrap();
        }
        z.finish().unwrap();
    }

    #[test]
    fn csharp_script_installer_flagged_plain_install() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        fs::create_dir_all(game_path.join("Data")).unwrap();
        let mut game = crate::game::find("Fallout4").unwrap();
        game.plugin.as_mut().unwrap().policy = None;
        game.gamebryo = None;
        let mut m = Manager::open(game, game_path.clone(), t.path().join("mods")).unwrap();

        let a = t.path().join("Scripted.zip");
        zip(
            &a,
            &[
                ("Scripted/fomod/Script.cs", "class Script {}"),
                ("Scripted/fomod/info.xml", "<fomod/>"),
                ("Scripted/S.esp", "p"),
            ],
        );
        let s = m.stage(&a).unwrap();
        assert!(s.fomod.is_none());
        assert_eq!(s.script.as_deref(), Some("Scripted/fomod/Script.cs"));
        m.install(s, None).unwrap();
        assert!(game_path.join("Data/S.esp").exists());
        assert!(!game_path.join("Data/fomod").exists());

        // An XML config wins: the FOMod wizard runs, nothing is flagged.
        let b = t.path().join("Both.zip");
        zip(
            &b,
            &[
                ("fomod/script.cs", "class Script {}"),
                (
                    "fomod/ModuleConfig.xml",
                    "<config><moduleName>B</moduleName></config>",
                ),
            ],
        );
        let s = m.stage(&b).unwrap();
        assert!(s.fomod.is_some() && s.script.is_none());
    }

    #[test]
    fn mod_script_runs_then_installs_its_plan() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        fs::create_dir_all(game_path.join("Data")).unwrap();
        fs::write(game_path.join("Data/Oblivion.esm"), "").unwrap();
        let user = t.path().join("user");
        fs::create_dir_all(&user).unwrap();
        let ini = user.join("Oblivion.ini");
        fs::write(&ini, "[Archive]\nSArchiveList=Base.bsa\n").unwrap();
        let txt = t
            .path()
            .join("appdata/plugins.txt")
            .to_string_lossy()
            .into_owned();
        let mut game = crate::game::find("Oblivion").unwrap();
        let gb = game.gamebryo.as_mut().unwrap();
        gb.user_game_data_path = Some(user.to_string_lossy().into());
        gb.plugins_file_path = Some(txt.clone());
        if let Some(p) = game.plugin.as_mut().and_then(|p| p.policy.as_mut()) {
            p.plugins_file_path = Some(txt);
        }
        let mut m = Manager::open(game, game_path.clone(), t.path().join("mods")).unwrap();

        let script = r#"DontInstallAnyDataFiles
If DialogYesNo "Extras?"
    CopyDataFile opt\x.txt extras\x.txt
EndIf
SetTextLine cfg.txt 0 "A"
EditINI General bTest 1
RegisterBSA Mine.bsa
UncheckEsp B.esp"#;
        let a = t.path().join("Scripted.zip");
        zip(
            &a,
            &[
                ("Wrap/fomod/script.txt", script),
                ("Wrap/A.esp", ""),
                ("Wrap/B.esp", ""),
                ("Wrap/opt/x.txt", "x"),
                ("Wrap/cfg.txt", "a\r\nb\r\n"),
            ],
        );
        let mut s = m.stage(&a).unwrap();
        assert!(s.fomod.is_none() && s.script.is_none());
        assert!(matches!(
            m.run_mod_script(&mut s, &[]).unwrap(),
            Some(modscript::Prompt::YesNo { .. })
        ));
        assert!(m.install(s.clone(), None).is_err()); // not run to the end yet
        let answers = [modscript::Answer::Bool(true)];
        assert_eq!(m.run_mod_script(&mut s, &answers).unwrap(), None);
        m.install(s, None).unwrap();

        let data = game_path.join("Data");
        assert_eq!(fs::read_to_string(data.join("extras/x.txt")).unwrap(), "x");
        assert_eq!(
            fs::read_to_string(data.join("cfg.txt")).unwrap(),
            "A\r\nb\r\n"
        );
        assert!(!data.join("opt").exists() && !data.join("fomod").exists());
        assert_eq!(ini::get(&ini, "General", "bTest").as_deref(), Some("1"));
        assert_eq!(
            ini::get(&ini, "Archive", "SArchiveList").as_deref(),
            Some("Base.bsa, Mine.bsa")
        );
        let active: Vec<(String, bool)> = m
            .plugins()
            .unwrap()
            .into_iter()
            .map(|r| (r.entry.name, r.entry.active))
            .collect();
        assert!(active.contains(&("A.esp".into(), true)));
        assert!(active.contains(&("B.esp".into(), false)));

        // Removing the mod undoes its INI edits.
        m.remove("Scripted").unwrap();
        assert_eq!(ini::get(&ini, "General", "bTest"), None);
        assert_eq!(
            ini::get(&ini, "Archive", "SArchiveList").as_deref(),
            Some("Base.bsa")
        );
    }

    #[test]
    fn fomod_metadata_screenshot_choices_and_plugin_states() {
        use crate::plugins::tests::tes4;
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        let data = game_path.join("Data");
        fs::create_dir_all(&data).unwrap();
        fs::write(game_path.join("SkyrimSE.exe"), "not a PE").unwrap();
        let txt = t.path().join("plugins.txt");
        let mut game = crate::game::find("SkyrimSE").unwrap();
        let policy = game.plugin.as_mut().unwrap().policy.as_mut().unwrap();
        policy.plugins_file_path = Some(txt.to_string_lossy().into());
        let mods = t.path().join("mods");
        let mut m = Manager::open(game, game_path, mods.clone()).unwrap();
        fs::write(data.join("On.esp"), tes4(0, &[], "")).unwrap();
        fs::write(data.join("Off.esp"), tes4(0, &[], "")).unwrap();
        fs::write(data.join("loose.txt"), "x").unwrap();
        fs::write(&txt, "*On.esp\nOff.esp\n").unwrap();
        let state = m.fomod_env().file_state;
        assert_eq!(state("On.esp"), "Active");
        assert_eq!(state("off.ESP"), "Inactive");
        assert_eq!(state("Gone.esp"), "Missing");
        assert_eq!(state("loose.txt"), "Active");
        // The executable isn't a PE file: version unknown, so a game version check fails.
        assert_eq!(m.fomod_env().game, None);

        let cfg = r#"<config><moduleName>Opt</moduleName><installSteps><installStep name="S">
  <optionalFileGroups><group name="G" type="SelectAny"><plugins order="Explicit">
    <plugin name="A"><description/><files><file source="a.txt"/></files><typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
    <plugin name="B"><description/><files><file source="b.txt"/></files><typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
  </plugins></group></optionalFileGroups></installStep></installSteps></config>"#;
        let a = t.path().join("Opt.zip");
        zip(
            &a,
            &[
                ("fomod/ModuleConfig.xml", cfg),
                ("fomod/info.xml", "<fomod><Name>Optional Things</Name><Author>Me</Author><Version>2.0</Version><Id>42</Id></fomod>"),
                ("fomod/screenshot.png", "png"),
                ("a.txt", "a"),
                ("b.txt", "b"),
            ],
        );
        let s = m.stage(&a).unwrap();
        assert_eq!(
            s.fomod.as_ref().unwrap().1.image.as_deref(),
            Some("fomod/screenshot.png")
        );
        assert_eq!(s.choices, None);
        m.install(s, Some(&vec![(0, 0, 1)])).unwrap();
        let e = &m.state.mods[0];
        assert_eq!(e.name, "Optional Things");
        let info = e.info.as_ref().unwrap();
        assert_eq!(
            (&info.author[..], &info.version[..], &info.id[..]),
            ("Me", "2.0", "42")
        );
        let shot = m.screenshot_path("Opt").unwrap();
        assert_eq!(fs::read_to_string(&shot).unwrap(), "png");

        // Uninstall keeps the choices; reinstalling offers them back.
        m.remove("Opt").unwrap();
        assert!(!shot.exists());
        let s = m.stage(&a).unwrap();
        assert_eq!(s.choices, Some(vec![(0, 0, 1)]));
        m.install(s.clone(), s.choices.as_ref()).unwrap();
        assert_eq!(m.state.mods[0].files, ["b.txt"]);

        // Unmet moduleDependencies refuse the install up front.
        let b = t.path().join("Needs.zip");
        zip(
            &b,
            &[(
                "fomod/ModuleConfig.xml",
                r#"<config><moduleName>N</moduleName><moduleDependencies operator="And">
                     <fileDependency file="Off.esp" state="Active"/></moduleDependencies></config>"#,
            )],
        );
        let err = m.stage(&b).unwrap_err().to_string();
        assert_eq!(err, "File 'Off.esp' is not Active.");
        assert!(!mods.join(".staging/Needs").exists());
    }

    #[test]
    fn readmes_kept_on_install_dropped_on_remove() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        fs::create_dir_all(game_path.join("Data")).unwrap();
        let mut game = crate::game::find("Fallout4").unwrap();
        game.plugin.as_mut().unwrap().policy = None;
        game.gamebryo = None;
        let mods = t.path().join("mods");
        let mut m = Manager::open(game, game_path, mods.clone()).unwrap();

        let a = t.path().join("Doc.zip");
        zip(
            &a,
            &[
                ("fomod/readme.txt", "installer notes"), // installer folder: skipped
                ("Doc/ReadMe.txt", "hello"),
                ("Doc/Guide.PDF", "%PDF"),
                ("Doc/script.txt", "s"),         // C# known non-readme
                ("Doc/MyConfig.txt", "c"),       // config file
                ("Doc/empty.txt", ""),           // C# skips empty files
                ("Doc/docs/changes.txt", "old"), // deeper than the top readmes
                ("Doc/D.esp", "p"),
            ],
        );
        let s = m.stage(&a).unwrap();
        m.install(s, None).unwrap();
        assert_eq!(m.state.mods[0].readmes, ["Doc/Guide.PDF", "Doc/ReadMe.txt"]);
        let p = m.readme_path("Doc", "Doc/ReadMe.txt").unwrap();
        assert_eq!(fs::read_to_string(p).unwrap(), "hello");
        // Only indexed names resolve: no reading outside the mod's readme folder.
        assert!(m.readme_path("Doc", "../../state.json").is_none());
        assert!(m.readme_path("Nope", "Doc/ReadMe.txt").is_none());

        m.remove("Doc").unwrap();
        assert!(!mods.join("ReadMe/Doc").exists());
    }

    fn nexus_meta(
        file_id: u64,
        file_name: &str,
        version: &str,
    ) -> crate::nexus::updates::NexusMeta {
        crate::nexus::updates::NexusMeta {
            domain: "skyrimspecialedition".into(),
            mod_id: 7,
            file_id,
            mod_name: "Sky".into(),
            file_name: file_name.into(),
            version: version.into(),
            author: String::new(),
            category_id: None,
            update: None,
            checked: None,
        }
    }

    #[test]
    fn upgrade_keeps_order_plugins_profiles() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        let data = game_path.join("Data");
        fs::create_dir_all(data.join("textures")).unwrap();
        fs::write(data.join("textures/sky.dds"), "vanilla").unwrap();
        let txt = t.path().join("plugins.txt");
        let mut game = crate::game::find("SkyrimSE").unwrap();
        let policy = game.plugin.as_mut().unwrap().policy.as_mut().unwrap();
        policy.plugins_file_path = Some(txt.to_string_lossy().into());
        let mods = t.path().join("mods");
        let mut m = Manager::open(game, game_path, mods.clone()).unwrap();
        let add = |m: &mut Manager, name: &str, files: &[(&str, &str)]| {
            let a = t.path().join(format!("{name}.zip"));
            zip(&a, files);
            m.stage(&a).unwrap()
        };
        let mut s = add(
            &mut m,
            "Sky-1.0",
            &[("A.esp", ""), ("textures/sky.dds", "A1"), ("old.txt", "o")],
        );
        s.nexus = Some(nexus_meta(100, "Main", "1.0"));
        m.install(s, None).unwrap();
        let s = add(&mut m, "C", &[("C.esp", ""), ("textures/sky.dds", "C")]);
        m.install(s, None).unwrap();
        // The user moved A.esp after C.esp and switched it off.
        fs::write(&txt, "*C.esp\r\nA.esp\r\n").unwrap();
        m.save_profile("p").unwrap();
        let ini = t.path().join("x.ini");
        fs::write(&ini, "[S]\nk=orig\n").unwrap();
        m.edit_ini("Sky-1.0", &ini, "S", "k", "mod");
        m.deploy().unwrap();
        assert_eq!(ini::get(&ini, "S", "k").as_deref(), Some("mod"));

        // Only the same Nexus file (by id, update chain or name) in another version matches.
        let mut s = add(
            &mut m,
            "Sky-2.0",
            &[("A.esp", ""), ("textures/sky.dds", "A2"), ("new.txt", "n")],
        );
        for (meta, hit) in [
            (nexus_meta(101, "Optional", "2.0"), false),
            (nexus_meta(100, "Main", "1.0"), false),
            (nexus_meta(101, "main", "2.0"), true),
        ] {
            s.nexus = Some(meta);
            assert_eq!(m.older_version(&s).is_some(), hit);
        }
        assert_eq!(m.older_version(&s).as_deref(), Some("Sky-1.0"));
        m.set_replaces(&mut s, "Sky-1.0");

        // Cancelled mid-deploy: the old version is back, the new one's files are gone.
        let task = Task::default();
        let flag = task.cancel.clone();
        m.task = Task {
            progress: Arc::new(move |l, _, _| {
                if l == "Deploying" {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed)
                }
            }),
            ..task
        };
        let r = m.transact(|m| m.install(s.clone(), None));
        assert!(matches!(r, Err(Error::Cancelled)));
        m.task = Task::default();
        let ids = |m: &Manager| {
            m.state
                .mods
                .iter()
                .map(|x| x.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&m), ["Sky-1.0", "C"]);
        assert_eq!(m.state.profiles["p"], ["Sky-1.0", "C"]);
        assert!(data.join("old.txt").exists() && !mods.join("VirtualInstall/Sky-2.0").exists());
        let header =
            "# This file is used by the game to keep track of your downloaded content.\r\n";
        // Rollback keeps the load order: A.esp stays below C.esp, inactive.
        assert_eq!(
            fs::read_to_string(&txt).unwrap(),
            format!("{header}*C.esp\r\nA.esp\r\n")
        );

        let s = add(
            &mut m,
            "Sky-2.0",
            &[("A.esp", ""), ("textures/sky.dds", "A2"), ("new.txt", "n")],
        );
        let mut s = Staged {
            nexus: Some(nexus_meta(101, "Main", "2.0")),
            ..s
        };
        m.set_replaces(&mut s, "Sky-1.0");
        m.install(s, None).unwrap();
        // Same slot, same plugin position and state, same profiles; files it dropped are gone.
        assert_eq!(ids(&m), ["Sky-2.0", "C"]);
        assert_eq!(m.state.mods[0].nexus.as_ref().unwrap().version, "2.0");
        assert_eq!(
            fs::read_to_string(&txt).unwrap(),
            format!("{header}*C.esp\r\nA.esp\r\n")
        );
        assert_eq!(m.state.profiles["p"], ["Sky-2.0", "C"]);
        assert!(!data.join("old.txt").exists());
        assert_eq!(fs::read_to_string(data.join("new.txt")).unwrap(), "n");
        assert!(!mods.join("VirtualInstall/Sky-1.0").exists());
        // The old version's INI edit is undone (the new one doesn't make it).
        assert_eq!(ini::get(&ini, "S", "k").as_deref(), Some("orig"));
        assert!(m.state.ini_edits.is_empty());
        // Conflicts: C still wins; without it the new version shows; without both, vanilla.
        let sky = || fs::read_to_string(data.join("textures/sky.dds")).unwrap();
        assert_eq!(sky(), "C");
        m.remove("C").unwrap();
        assert_eq!(sky(), "A2");
        m.set_active("Sky-2.0", false).unwrap();
        assert_eq!(sky(), "vanilla");
    }

    #[test]
    fn reinstall_and_upgrade_replay_fomod_choices() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        let data = game_path.join("Data");
        fs::create_dir_all(&data).unwrap();
        let mut game = crate::game::find("Fallout4").unwrap();
        game.plugin.as_mut().unwrap().policy = None;
        game.gamebryo = None;
        let mods = t.path().join("mods");
        let mut m = Manager::open(game, game_path, mods.clone()).unwrap();
        m.set_link_mode(LinkMode::Copy).unwrap();
        let cfg = r#"<config><moduleName>Opt</moduleName><installSteps><installStep name="S">
  <optionalFileGroups><group name="G" type="SelectExactlyOne"><plugins order="Explicit">
    <plugin name="A"><description/><files><file source="a.txt"/></files><typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
    <plugin name="B"><description/><files><file source="b.txt"/></files><typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
  </plugins></group></optionalFileGroups></installStep></installSteps></config>"#;
        let archive = |name: &str, v: &str| {
            let p = t.path().join(name);
            let info = format!("<fomod><Name>Opt</Name><Version>{v}</Version></fomod>");
            zip(
                &p,
                &[
                    ("fomod/ModuleConfig.xml", cfg),
                    ("fomod/info.xml", &info),
                    ("fomod/screenshot.png", v),
                    ("readme.txt", v),
                    ("a.txt", &format!("a{v}")),
                    ("b.txt", &format!("b{v}")),
                ],
            );
            p
        };
        let a = archive("Opt.zip", "1");
        let s = m.stage(&a).unwrap();
        assert_eq!(s.replaces, None);
        m.install(s, Some(&vec![(0, 0, 1)])).unwrap();

        // Same archive again: a reinstall of the installed mod, offering its choices.
        let a = archive("Opt.zip", "1.1");
        let s = m.stage(&a).unwrap();
        assert_eq!(s.replaces.as_deref(), Some("Opt"));
        assert_eq!(s.choices, Some(vec![(0, 0, 1)]));
        let err = m.install(
            Staged {
                replaces: None,
                ..s.clone()
            },
            Some(&vec![(0, 0, 0)]),
        );
        assert!(err.unwrap_err().to_string().contains("already installed"));
        let s = m.stage(&a).unwrap();
        m.install(s, Some(&vec![(0, 0, 0)])).unwrap();
        assert_eq!(m.state.mods.len(), 1);
        assert_eq!(m.state.mods[0].files, ["a.txt"]);
        // Copied (not linked) files are refreshed too; the dropped option's file goes.
        assert_eq!(fs::read_to_string(data.join("a.txt")).unwrap(), "a1.1");
        assert!(!data.join("b.txt").exists() && !mods.join("VirtualInstall/Opt/b.txt").exists());
        assert_eq!(m.state.fomod_choices["Opt"][0].plugin, "A");

        // Upgrade to a differently named archive: the old mod's choices are replayed.
        let b = archive("Opt v2.zip", "2");
        let mut s = m.stage(&b).unwrap();
        assert_eq!((s.replaces.as_deref(), s.choices.as_ref()), (None, None));
        m.set_replaces(&mut s, "Opt");
        assert_eq!(s.choices, Some(vec![(0, 0, 0)]));
        let sel = s.choices.clone();
        m.install(s, sel.as_ref()).unwrap();
        let e = &m.state.mods[0];
        assert_eq!(
            (&e.id[..], &e.info.as_ref().unwrap().version[..]),
            ("Opt v2", "2")
        );
        assert_eq!(fs::read_to_string(data.join("a.txt")).unwrap(), "a2");
        let readme = m.readme_path("Opt v2", "readme.txt").unwrap();
        assert_eq!(fs::read_to_string(readme).unwrap(), "2");
        assert_eq!(
            fs::read_to_string(m.screenshot_path("Opt v2").unwrap()).unwrap(),
            "2"
        );
        assert!(!mods.join("ReadMe/Opt").exists() && !mods.join("Screenshot/Opt").exists());
    }

    #[test]
    fn ini_edits_stack_and_restore() {
        let t = tempfile::tempdir().unwrap();
        let lib = t.path().join("steamapps");
        let game_path = lib.join("common/Fallout 4");
        fs::create_dir_all(game_path.join("Data")).unwrap();
        let mut game = crate::game::find("Fallout4").unwrap();
        game.plugin.as_mut().unwrap().policy = None;
        let appid = game
            .discovery
            .stores
            .iter()
            .find(|s| s.store == "Steam")
            .and_then(|s| s.id.clone())
            .unwrap();
        let user = lib.join(format!("compatdata/{appid}/pfx/drive_c/users/steamuser"));
        fs::create_dir_all(&user).unwrap();
        if cfg!(windows) {
            game.user_dir = Some(user.clone()); // no Proton there
        }
        let mods = t.path().join("mods");
        let mut m = Manager::open(game, game_path.clone(), mods.clone()).unwrap();
        // The game INI resolves into the Proton prefix.
        let ini = m.settings_file(None).unwrap();
        assert_eq!(
            ini,
            user.join("Documents/My Games/Fallout4/Fallout4Custom.ini")
        );
        fs::create_dir_all(ini.parent().unwrap()).unwrap();
        fs::write(&ini, "[Display]\r\niSize W=1280\r\n").unwrap();
        for id in ["A", "B"] {
            let a = t.path().join(format!("{id}.zip"));
            zip(&a, &[(&format!("{id}.esp"), "p")]);
            let s = m.stage(&a).unwrap();
            m.install(s, None).unwrap();
        }
        let get = |s: &str, k: &str| ini::get(&ini, s, k);

        m.edit_ini("A", &ini, "Display", "iSize W", "1920");
        m.edit_ini("A", &ini, "General", "bNew", "1");
        m.deploy().unwrap();
        assert_eq!(get("Display", "iSize W").as_deref(), Some("1920"));
        assert_eq!(get("General", "bNew").as_deref(), Some("1"));
        // Last editor wins; disabling it falls back to the one below, enabling re-applies it.
        m.edit_ini("B", &ini, "display", "ISIZE W", "2560");
        m.deploy().unwrap();
        assert_eq!(get("Display", "iSize W").as_deref(), Some("2560"));
        m.set_active("B", false).unwrap();
        assert_eq!(get("Display", "iSize W").as_deref(), Some("1920"));
        m.set_active("B", true).unwrap();
        assert_eq!(get("Display", "iSize W").as_deref(), Some("2560"));
        // Recorded across restarts.
        let mut m = Manager::open(m.game.clone(), game_path, mods).unwrap();
        // Removing a mod that doesn't own the live value leaves it; keys only it added go.
        m.remove("A").unwrap();
        assert_eq!(get("Display", "iSize W").as_deref(), Some("2560"));
        assert_eq!(get("General", "bNew"), None);
        // Editing again replaces the mod's value (upgrade), no second entry.
        m.edit_ini("B", &ini, "Display", "iSize W", "3000");
        m.deploy().unwrap();
        assert_eq!(get("Display", "iSize W").as_deref(), Some("3000"));
        assert_eq!(m.state.ini_edits.len(), 1);
        assert_eq!(m.state.ini_edits[0].by.len(), 1);
        // Last editor gone: the original value is back and nothing is left recorded.
        m.remove("B").unwrap();
        assert_eq!(get("Display", "iSize W").as_deref(), Some("1280"));
        assert!(m.state.ini_edits.is_empty());
    }

    #[test]
    fn install_conflict_backup_and_restore() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        let data = game_path.join("Data");
        fs::create_dir_all(data.join("textures")).unwrap();
        fs::write(data.join("textures/sky.dds"), "vanilla").unwrap();

        let mut game = crate::game::find("Fallout4").unwrap();
        game.plugin.as_mut().unwrap().policy = None; // no plugins.txt in the test sandbox
        game.gamebryo = None;
        let mut m = Manager::open(game, game_path, t.path().join("mods")).unwrap();

        let a = t.path().join("ModA.zip");
        zip(
            &a,
            &[
                ("ModA v1/textures/sky.dds", "A"),
                ("ModA v1/A.esp", "p"),
                ("ModA v1/meta.ini", "x"),
            ],
        );
        let b = t.path().join("ModB.zip");
        zip(&b, &[("Data/textures/sky.dds", "B")]);

        let s = m.stage(&a).unwrap();
        m.install(s, None).unwrap();
        assert_eq!(
            fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
            "A"
        );
        assert!(data.join("A.esp").exists() && !data.join("meta.ini").exists());

        let s = m.stage(&b).unwrap();
        m.install(s, None).unwrap();
        assert_eq!(
            fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
            "B"
        );
        assert_eq!(m.conflicts()["ModA"], vec!["ModB"]);

        m.reorder("ModB", -1).unwrap();
        assert_eq!(
            fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
            "A"
        );

        m.remove("ModA").unwrap();
        m.set_active("ModB", false).unwrap();
        assert_eq!(
            fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
            "vanilla"
        );
        assert!(!data.join("A.esp").exists());

        m.save_profile("b-on").unwrap();
        m.set_active("ModB", true).unwrap();
        m.load_profile("b-on").unwrap();
        assert_eq!(
            fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
            "vanilla"
        );

        let reopened =
            Manager::open(m.game.clone(), m.game_path.clone(), m.mods_dir.clone()).unwrap();
        assert_eq!(reopened.state.mods.len(), 1);

        fs::write(m.mods_dir.join("state.json"), "").unwrap(); // torn write after power cut
        let reopened =
            Manager::open(m.game.clone(), m.game_path.clone(), m.mods_dir.clone()).unwrap();
        assert_eq!(reopened.state.mods.len(), 1);
    }

    #[test]
    fn deployed_plugins_join_load_order() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        let data = game_path.join("Data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("Skyrim.esm"), "").unwrap();
        let txt = t.path().join("appdata/plugins.txt");
        let mut game = crate::game::find("SkyrimSE").unwrap();
        let policy = game.plugin.as_mut().unwrap().policy.as_mut().unwrap();
        policy.plugins_file_path = Some(txt.to_string_lossy().into());
        let mut m = Manager::open(game, game_path, t.path().join("mods")).unwrap();
        let mut add = |name: &str, files: &[(&str, &str)]| {
            let a = t.path().join(format!("{name}.zip"));
            zip(&a, files);
            let s = m.stage(&a).unwrap();
            m.install(s, None).unwrap();
        };
        add("ModA", &[("A.esp", "")]);
        add("ModB", &[("B.esp", ""), ("B.esm", "")]);
        let body = || fs::read_to_string(&txt).unwrap();
        let header =
            "# This file is used by the game to keep track of your downloaded content.\r\n";
        assert_eq!(body(), format!("{header}*B.esm\r\n*A.esp\r\n*B.esp\r\n"));

        // The user's order and choices survive later deploys; a new plugin loads last, active.
        fs::write(&txt, "*B.esm\r\n*B.esp\r\nA.esp\r\n").unwrap();
        add("ModC", &[("C.esp", "")]);
        assert_eq!(
            body(),
            format!("{header}*B.esm\r\n*B.esp\r\nA.esp\r\n*C.esp\r\n")
        );
        // Disabling a mod drops its plugins; enabling it again appends them.
        m.set_active("ModB", false).unwrap();
        assert_eq!(body(), format!("{header}A.esp\r\n*C.esp\r\n"));
        m.set_active("ModB", true).unwrap();
        assert_eq!(
            body(),
            format!("{header}*B.esm\r\nA.esp\r\n*C.esp\r\n*B.esp\r\n")
        );
    }

    #[test]
    fn deployed_plugins_missing_masters_stay_inactive() {
        use crate::plugins::tests::tes4;
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        fs::create_dir_all(game_path.join("Data")).unwrap();
        let txt = t.path().join("plugins.txt");
        let mut game = crate::game::find("SkyrimSE").unwrap();
        let policy = game.plugin.as_mut().unwrap().policy.as_mut().unwrap();
        policy.plugins_file_path = Some(txt.to_string_lossy().into());
        let mut m = Manager::open(game, game_path, t.path().join("mods")).unwrap();
        let mut add = |name: &str, files: &[(&str, Vec<u8>)]| {
            let a = t.path().join(format!("{name}.zip"));
            let mut z = zip::ZipWriter::new(fs::File::create(&a).unwrap());
            for (n, body) in files {
                z.start_file(*n, zip::write::SimpleFileOptions::default())
                    .unwrap();
                z.write_all(body).unwrap();
            }
            z.finish().unwrap();
            let s = m.stage(&a).unwrap();
            m.install(s, None).unwrap();
        };
        add("Orphan", &[("Orphan.esp", tes4(0, &["Nowhere.esm"], ""))]);
        // Archive order puts the dependent first; its master still loads above it and both activate.
        add(
            "Pair",
            &[
                ("Dep.esp", tes4(0, &["Lib.esp"], "")),
                ("Lib.esp", tes4(1, &[], "")),
            ],
        );
        let body = fs::read_to_string(&txt).unwrap();
        assert!(
            body.ends_with("*Lib.esp\r\nOrphan.esp\r\n*Dep.esp\r\n"),
            "{body}"
        );
    }

    #[test]
    fn plugin_tab_reorders_toggles_and_refuses_breakage() {
        use crate::plugins::{tests::tes4, Issue, Plugin};
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        let data = game_path.join("Data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("Skyrim.esm"), tes4(1, &[], "")).unwrap();
        fs::write(data.join("Lib.esm"), tes4(1, &[], "")).unwrap();
        fs::write(data.join("Dep.esp"), tes4(0, &["Lib.esm"], "Needs Lib")).unwrap();
        fs::write(data.join("Free.esp"), tes4(0, &[], "")).unwrap();
        fs::write(data.join("Broken.esp"), tes4(0, &["Gone.esm"], "")).unwrap();
        let txt = t.path().join("plugins.txt");
        fs::write(&txt, "*Lib.esm\r\n*Dep.esp\r\n*Free.esp\r\n*Broken.esp\r\n").unwrap();
        let mut game = crate::game::find("SkyrimSE").unwrap();
        let policy = game.plugin.as_mut().unwrap().policy.as_mut().unwrap();
        policy.plugins_file_path = Some(txt.to_string_lossy().into());
        let m = Manager::open(game, game_path, t.path().join("mods")).unwrap();

        let rows = m.plugins().unwrap();
        let view = |rows: &[PluginRow]| -> Vec<String> {
            rows.iter()
                .map(|r| format!("{}{}", if r.entry.active { "*" } else { "" }, r.entry.name))
                .collect()
        };
        assert_eq!(
            view(&rows),
            [
                "*Skyrim.esm",
                "*Lib.esm",
                "*Dep.esp",
                "*Free.esp",
                "*Broken.esp"
            ]
        );
        assert!(rows[0].locked_order && rows[0].locked_active && !rows[1].locked_order);
        assert_eq!(
            rows[2].entry.header.description.as_deref(),
            Some("Needs Lib")
        );
        assert_eq!(rows[4].entry.diagnostics[0].issue, Issue::MissingMaster);
        let pl = |n: &str, active: bool| Plugin {
            name: n.into(),
            active,
        };

        // Dragging Skyrim.esm down or Dep above its master is corrected, not refused.
        // Broken.esp's existing error doesn't block unrelated changes (turning Free off).
        let rows = m
            .set_plugins(&[
                pl("dep.esp", true),
                pl("Free.esp", false),
                pl("Lib.esm", true),
                pl("Skyrim.esm", true),
                pl("Broken.esp", true),
            ])
            .unwrap();
        assert_eq!(
            view(&rows),
            [
                "*Skyrim.esm",
                "*Lib.esm",
                "*Dep.esp",
                "Free.esp",
                "*Broken.esp"
            ]
        );
        assert_eq!(
            fs::read_to_string(&txt).unwrap(),
            "# This file is used by the game to keep track of your downloaded content.\r\n\
             *Lib.esm\r\n*Dep.esp\r\nFree.esp\r\n*Broken.esp\r\n"
        );

        // Deactivating a master in use is refused; the files stay as they were.
        let before = fs::read_to_string(&txt).unwrap();
        let err = m
            .set_plugins(&[pl("Lib.esm", false), pl("Dep.esp", true)])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Dep.esp: Required master is inactive: Lib.esm");
        assert_eq!(fs::read_to_string(&txt).unwrap(), before);
        // ...together with its dependent it's fine. Skyrim.esm can't be turned off.
        let rows = m
            .set_plugins(&[
                pl("Skyrim.esm", false),
                pl("Lib.esm", false),
                pl("Dep.esp", false),
            ])
            .unwrap();
        assert_eq!(
            view(&rows),
            [
                "*Skyrim.esm",
                "Lib.esm",
                "Dep.esp",
                "Free.esp",
                "*Broken.esp"
            ]
        );
        // Activating a plugin whose master is off is refused.
        assert!(m.set_plugins(&[pl("Dep.esp", true)]).is_err());
    }

    #[test]
    fn profiles_keep_plugin_order_and_active_state() {
        use crate::plugins::{tests::tes4, Plugin};
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        fs::create_dir_all(game_path.join("Data")).unwrap();
        let txt = t.path().join("plugins.txt");
        let mut game = crate::game::find("SkyrimSE").unwrap();
        let policy = game.plugin.as_mut().unwrap().policy.as_mut().unwrap();
        policy.plugins_file_path = Some(txt.to_string_lossy().into());
        let mut m = Manager::open(game, game_path, t.path().join("mods")).unwrap();
        let add = |m: &mut Manager, name: &str, files: &[(&str, Vec<u8>)]| {
            let a = t.path().join(format!("{name}.zip"));
            let mut z = zip::ZipWriter::new(fs::File::create(&a).unwrap());
            for (n, body) in files {
                z.start_file(*n, zip::write::SimpleFileOptions::default())
                    .unwrap();
                z.write_all(body).unwrap();
            }
            z.finish().unwrap();
            let s = m.stage(&a).unwrap();
            m.install(s, None).unwrap();
        };
        add(&mut m, "ModA", &[("A.esp", tes4(0, &[], ""))]);
        add(&mut m, "ModB", &[("B.esp", tes4(0, &[], ""))]);
        add(&mut m, "Lib", &[("Lib.esm", tes4(1, &[], ""))]);
        add(&mut m, "Dep", &[("Dep.esp", tes4(0, &["Lib.esm"], ""))]);
        let pl = |n: &str, active: bool| Plugin {
            name: n.into(),
            active,
        };
        let body = || {
            fs::read_to_string(&txt)
                .unwrap()
                .lines()
                .skip(1)
                .collect::<Vec<_>>()
                .join(" ")
        };
        m.set_plugins(&[
            pl("Lib.esm", true),
            pl("B.esp", true),
            pl("A.esp", false),
            pl("Dep.esp", true),
        ])
        .unwrap();
        m.save_profile("one").unwrap();
        m.set_plugins(&[pl("A.esp", true), pl("B.esp", true), pl("Dep.esp", false)])
            .unwrap();
        m.set_active("ModB", false).unwrap();
        m.save_profile("two").unwrap();
        assert_eq!(body(), "*Lib.esm *A.esp Dep.esp");

        m.load_profile("one").unwrap();
        assert_eq!(body(), "*Lib.esm *B.esp A.esp *Dep.esp");
        m.load_profile("two").unwrap();
        assert_eq!(body(), "*Lib.esm *A.esp Dep.esp");
        // Saved state survives a reopen.
        let m2 = Manager::open(m.game.clone(), m.game_path.clone(), m.mods_dir.clone()).unwrap();
        assert_eq!(m2.state.profile_plugins["one"].len(), 4);

        // A plugin gone since the save is skipped.
        m.remove("ModA").unwrap();
        m.load_profile("one").unwrap();
        assert_eq!(body(), "*Lib.esm *B.esp *Dep.esp");

        // A profile saved before plugins were kept only switches mods.
        m.state.profiles.insert("old".into(), vec!["ModB".into()]);
        m.load_profile("old").unwrap();
        assert_eq!(body(), "*B.esp");

        // Order that would break the load order (Dep active, its master gone) is refused and
        // the switch rolled back.
        m.load_profile("one").unwrap();
        m.remove("Lib").unwrap();
        m.set_active("Dep", false).unwrap();
        let before = (
            body(),
            m.state.mods.iter().map(|x| x.active).collect::<Vec<_>>(),
        );
        let err = m.load_profile("one").unwrap_err().to_string();
        assert!(err.contains("Dep.esp"), "{err}");
        assert_eq!(
            before,
            (body(), m.state.mods.iter().map(|x| x.active).collect())
        );

        m.delete_profile("one").unwrap();
        assert!(!m.state.profile_plugins.contains_key("one"));
    }

    #[test]
    fn load_order_export_and_import() {
        use crate::plugins::tests::tes4;
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        let data = game_path.join("Data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("Skyrim.esm"), tes4(1, &[], "")).unwrap();
        fs::write(data.join("Lib.esm"), tes4(1, &[], "")).unwrap();
        fs::write(data.join("Dep.esp"), tes4(0, &["Lib.esm"], "")).unwrap();
        fs::write(data.join("A.esp"), tes4(0, &[], "")).unwrap();
        fs::write(data.join("B.esp"), tes4(0, &[], "")).unwrap();
        let txt = t.path().join("plugins.txt");
        fs::write(&txt, "*Lib.esm\r\n*Dep.esp\r\nA.esp\r\n*B.esp\r\n").unwrap();
        let mut game = crate::game::find("SkyrimSE").unwrap();
        let policy = game.plugin.as_mut().unwrap().policy.as_mut().unwrap();
        policy.plugins_file_path = Some(txt.to_string_lossy().into());
        let m = Manager::open(game, game_path, t.path().join("mods")).unwrap();
        let body = || {
            fs::read_to_string(&txt)
                .unwrap()
                .lines()
                .skip(1)
                .collect::<Vec<_>>()
                .join(" ")
        };

        let exported = m.export_load_order().unwrap();
        assert_eq!(
            exported,
            "GameMode=SkyrimSE\r\nSkyrim.esm=1\r\nLib.esm=1\r\nDep.esp=1\r\nA.esp=0\r\nB.esp=1\r\n"
        );

        // Listed plugins take that order and state; a bare name only moves; unknown ones are
        // reported; unlisted ones keep their state and go last.
        let r = m
            .import_load_order("GameMode=SkyrimSE\n\nb.esp=0\nA.esp=1\nGone.esp=1\nDep.esp\n")
            .unwrap();
        assert_eq!((r.total, r.imported), (4, 3));
        assert_eq!(r.unknown, ["Gone.esp"]);
        assert_eq!(body(), "*Lib.esm B.esp *A.esp *Dep.esp");

        // The export restores the earlier state.
        m.import_load_order(&exported).unwrap();
        assert_eq!(body(), "*Lib.esm *Dep.esp A.esp *B.esp");

        let err = |text: &str| m.import_load_order(text).unwrap_err().to_string();
        assert!(err("A.esp=1\n").contains("no game mode"));
        assert!(err("GameMode=Fallout4\nA.esp=1\n").contains("Fallout4"));
        assert!(err("GameMode=SkyrimSE\nGameMode=SkyrimSE\n").contains("more than one"));
        assert!(err("GameMode=SkyrimSE\nreadme.txt\n").contains("No plugins"));
        assert!(err("GameMode=SkyrimSE\nX.esp=1\n").contains("None of the plugins"));
        // Breaking the load order is refused as with any other change.
        let before = body();
        assert!(err("GameMode=SkyrimSE\nLib.esm=0\n").contains("Dep.esp"));
        assert_eq!(body(), before);
    }

    #[test]
    fn loot_preview_proposes_without_touching_the_game() {
        use crate::plugins::tests::tes4;
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        let data = game_path.join("Data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("Skyrim.esm"), tes4(1, &[], "")).unwrap();
        fs::write(data.join("Patch.esp"), tes4(0, &["Skyrim.esm"], "")).unwrap();
        fs::write(data.join("Mod.esp"), tes4(0, &["Skyrim.esm"], "")).unwrap();
        let txt = t.path().join("plugins.txt");
        fs::write(&txt, "*Patch.esp\r\n*Mod.esp\r\n").unwrap();
        let mut game = crate::game::find("SkyrimSE").unwrap();
        let policy = game.plugin.as_mut().unwrap().policy.as_mut().unwrap();
        policy.plugins_file_path = Some(txt.to_string_lossy().into());
        let mods = t.path().join("mods");
        // A fresh saved masterlist: no download.
        fs::create_dir_all(mods.join("loot")).unwrap();
        fs::write(
            mods.join("loot/masterlist.yaml"),
            "plugins:\n  - name: 'Patch.esp'\n    after: [ 'Mod.esp' ]\n",
        )
        .unwrap();
        let m = Manager::open(game, game_path, mods).unwrap();
        let before = fs::read_to_string(&txt).unwrap();
        let p = m.loot_preview().unwrap();
        assert_eq!(p.current, ["Skyrim.esm", "Patch.esp", "Mod.esp"]);
        assert_eq!(p.sorted, ["Skyrim.esm", "Mod.esp", "Patch.esp"]);
        assert_eq!(p.note, None);
        assert_eq!(fs::read_to_string(&txt).unwrap(), before);
    }

    /// Cancel at the Nth progress step: extract leaves no staging junk; a cancelled install,
    /// toggle or relink rolls the game and state back to exactly how they were.
    #[test]
    fn cancel_rolls_back() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("game/Data");
        fs::create_dir_all(data.join("textures")).unwrap();
        fs::write(data.join("textures/sky.dds"), "vanilla").unwrap();
        let mut game = crate::game::find("Fallout4").unwrap();
        game.plugin.as_mut().unwrap().policy = None;
        game.gamebryo = None;
        let mut m = Manager::open(game, t.path().join("game"), t.path().join("mods")).unwrap();
        let a = t.path().join("ModA.zip");
        zip(&a, &[("textures/sky.dds", "A"), ("textures/a.dds", "a")]);
        let b = t.path().join("ModB.zip");
        zip(
            &b,
            &[
                ("textures/sky.dds", "B"),
                ("textures/b.dds", "b"),
                ("textures/c.dds", "c"),
            ],
        );
        let s = m.stage(&a).unwrap();
        m.install(s, None).unwrap();

        // Cancel once `n` steps of `label` have reported; rollback steps are counted too.
        let rollback_steps = Arc::new(AtomicU64::new(0));
        let cancel_after = |m: &mut Manager, label: &'static str, n: u64| {
            let task = Task::default();
            let (flag, seen, rb) = (
                task.cancel.clone(),
                Arc::new(AtomicU64::new(0)),
                rollback_steps.clone(),
            );
            m.task = Task {
                progress: Arc::new(move |l, _, _| {
                    if l == "Rolling back" {
                        rb.fetch_add(1, Ordering::Relaxed);
                    }
                    if l == label && seen.fetch_add(1, Ordering::Relaxed) + 1 == n {
                        flag.store(true, Ordering::Relaxed)
                    }
                }),
                ..task
            };
        };
        let sky = || fs::read_to_string(data.join("textures/sky.dds")).unwrap();
        let unchanged = |m: &Manager| {
            assert!(m.in_sync() && !m.journal_path().exists());
            assert_eq!(
                m.state
                    .mods
                    .iter()
                    .map(|x| (x.id.as_str(), x.active))
                    .collect::<Vec<_>>(),
                [("ModA", true)]
            );
            assert_eq!(sky(), "A");
            assert!(data.join("textures/a.dds").exists() && !data.join("textures/b.dds").exists());
            let reopened =
                Manager::open(m.game.clone(), m.game_path.clone(), m.mods_dir.clone()).unwrap();
            assert_eq!(reopened.state.deployed, m.state.deployed);
        };

        cancel_after(&mut m, "Extracting", 1);
        assert_eq!(m.stage(&b).unwrap_err().to_string(), "cancelled");
        assert!(!m.mods_dir.join(".staging/ModB").exists());

        // Install: cancelled mid-copy, then mid-deploy (after ModA's sky.dds was unlinked).
        for (label, n) in [("Copying mod files", 1), ("Deploying", 1), ("Deploying", 3)] {
            cancel_after(&mut m, label, n);
            let s = m.stage(&b).unwrap();
            assert_eq!(
                m.transact(|m| m.install(s, None)).unwrap_err().to_string(),
                "cancelled",
                "{label} {n}"
            );
            unchanged(&m);
            assert!(!m.virtual_dir("ModB").exists());
        }

        cancel_after(&mut m, "Deploying", 1);
        assert_eq!(
            m.transact(|m| m.set_active("ModA", false))
                .unwrap_err()
                .to_string(),
            "cancelled"
        );
        unchanged(&m);

        cancel_after(&mut m, "Deploying", 1);
        assert_eq!(
            m.transact(|m| m.set_link_mode(LinkMode::Copy))
                .unwrap_err()
                .to_string(),
            "cancelled"
        );
        unchanged(&m);
        assert_eq!(m.state.link_mode, LinkMode::Auto);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let links = |p: &str| fs::metadata(data.join(p)).unwrap().nlink();
            assert!(
                links("textures/sky.dds") > 1 && links("textures/a.dds") > 1,
                "relinked back to hardlinks"
            );
        }
        assert!(rollback_steps.load(Ordering::Relaxed) > 0);

        m.task = Task::default();
        m.transact(|m| m.remove("ModA")).unwrap();
        assert_eq!(sky(), "vanilla");

        assert!(!m.virtual_dir("ModA").exists() && !m.mods_dir.join(".pending/ModA").exists());

        // Killed mid-install-copy: the marked, unlisted mod is deleted on the next open; an
        // unmarked folder the state doesn't know about is left alone.
        let reopen = |m: &Manager| {
            Manager::open(m.game.clone(), m.game_path.clone(), m.mods_dir.clone()).unwrap()
        };
        fs::create_dir_all(m.mods_dir.join("VirtualInstall/ModB/textures")).unwrap();
        fs::create_dir_all(m.mods_dir.join("VirtualInstall/Stranger")).unwrap();
        m.mark_pending("ModB").unwrap();
        reopen(&m);
        assert!(!m.virtual_dir("ModB").exists() && m.virtual_dir("Stranger").exists());

        // A lost state.json (or a stale .bak) never costs installed mods their files.
        m.transact(|m| m.install(m.stage(&a)?, None)).unwrap();
        fs::remove_file(m.mods_dir.join("state.json")).unwrap();
        fs::remove_file(m.mods_dir.join("state.json.bak")).unwrap();
        assert!(reopen(&m).state.mods.is_empty());
        assert!(m.virtual_dir("ModA").join("textures/sky.dds").exists());
    }

    /// Kill deploy after every possible filesystem step; reopen must leave state matching disk
    /// and lose no game file, and a follow-up deploy must still converge. Repeated with the
    /// journal garbled, so recovery falls back to scanning the game.
    #[test]
    fn crash_mid_deploy_recovers() {
        // (active before, sky.dds before, active after, sky.dds after)
        let cases: [(&[&str], &str, &[&str], &str); 3] = [
            (&["ModA"], "A", &["ModB"], "B"),
            (&["ModA"], "A", &[], "vanilla"),
            (&[], "vanilla", &["ModA"], "A"),
        ];
        // Copy mode exercises kills between a copy's tmp write and its rename.
        let runs = [
            (LinkMode::Auto, false),
            (LinkMode::Copy, false),
            (LinkMode::Auto, true),
            (LinkMode::Copy, true),
        ];
        for ((mode, garble), (before, was, after, want)) in
            runs.into_iter().flat_map(|r| cases.map(|c| (r, c)))
        {
            let mut crashes = 0;
            for n in 0.. {
                let t = tempfile::tempdir().unwrap();
                let data = t.path().join("game/Data");
                fs::create_dir_all(data.join("textures")).unwrap();
                fs::write(data.join("textures/sky.dds"), "vanilla").unwrap();
                let mut game = crate::game::find("Fallout4").unwrap();
                game.plugin.as_mut().unwrap().policy = None;
                game.gamebryo = None;
                let mut m =
                    Manager::open(game, t.path().join("game"), t.path().join("mods")).unwrap();
                m.state.link_mode = mode;
                for (name, files) in [
                    ("ModA", &[("textures/sky.dds", "A"), ("A.esp", "p")][..]),
                    ("ModB", &[("textures/sky.dds", "B")]),
                ] {
                    let z = t.path().join(format!("{name}.zip"));
                    zip(&z, files);
                    let s = m.stage(&z).unwrap();
                    m.install(s, None).unwrap();
                }
                let set = |m: &mut Manager, ids: &[&str]| {
                    m.state
                        .mods
                        .iter_mut()
                        .for_each(|x| x.active = ids.contains(&x.id.as_str()))
                };
                set(&mut m, before);
                m.deploy().unwrap();

                set(&mut m, after);
                CRASH_AFTER.with(|c| c.set(n));
                let crashed =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| m.deploy())).is_err();
                CRASH_AFTER.with(|c| c.set(usize::MAX));
                let (game, game_path, mods_dir) =
                    (m.game.clone(), m.game_path.clone(), m.mods_dir.clone());
                drop(m);
                if garble && mods_dir.join("deploy.journal").exists() {
                    fs::write(mods_dir.join("deploy.journal"), "{\"textures/sky.d").unwrap();
                }

                let mut m = Manager::open(game, game_path, mods_dir).unwrap();
                assert!(!m.journal_path().exists());
                // Reopen finishes the job: the game holds the saved intent, all of it.
                assert!(m.in_sync(), "{mode:?} n={n}");
                assert_eq!(
                    fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
                    if crashed { was } else { want },
                    "{mode:?} n={n}"
                );
                // A lost journal can't tell a fresh copy-mode A.esp from a game file: it's kept, untracked.
                let stray_esp = garble
                    && mode == LinkMode::Copy
                    && data.join("A.esp").exists()
                    && !m.state.deployed.contains_key("a.esp");
                assert_eq!(
                    data.join("A.esp").exists(),
                    stray_esp || (if crashed { before } else { after }).contains(&"ModA"),
                    "{mode:?} n={n}"
                );
                let strays = walkdir::WalkDir::new(t.path())
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".nmm-tmp"))
                    .count();
                assert_eq!(strays, 0, "{mode:?} n={n}");
                for d in m.state.deployed.values() {
                    assert!(
                        same_content(&m.virtual_dir(&d.mod_id).join(&d.path), &data.join(&d.path)),
                        "n={n} {d:?}"
                    );
                }
                let vanilla = [
                    data.join("textures/sky.dds"),
                    m.mods_dir.join("Backup/textures/sky.dds"),
                ]
                .iter()
                .filter(|p| fs::read_to_string(p).is_ok_and(|s| s == "vanilla"))
                .count();
                assert_eq!(vanilla, 1, "n={n}: game file lost or duplicated");

                set(&mut m, after);
                m.deploy().unwrap();
                assert_eq!(
                    fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
                    want,
                    "n={n}"
                );
                set(&mut m, &[]);
                m.deploy().unwrap();
                assert_eq!(
                    fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
                    "vanilla",
                    "n={n}"
                );
                assert!(stray_esp || !data.join("A.esp").exists());
                assert!(!m.mods_dir.join("Backup/textures/sky.dds").exists());

                if !crashed {
                    break;
                }
                crashes += 1;
            }
            assert!(crashes > 0);
        }
    }

    /// Journal and deploy records both lost: repair rebuilds them from disk, puts back a stranded
    /// game file, flags a backup the game has moved past, and doesn't claim an untouched game
    /// file just because an inactive mod ships the same bytes.
    #[test]
    fn repair_rebuilds_from_disk() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("game/Data");
        fs::create_dir_all(data.join("textures")).unwrap();
        fs::write(data.join("textures/sky.dds"), "vanilla").unwrap();
        fs::write(data.join("textures/same.dds"), "same").unwrap();
        fs::write(data.join("textures/patched.dds"), "v1").unwrap();
        let mut game = crate::game::find("Fallout4").unwrap();
        game.plugin.as_mut().unwrap().policy = None;
        game.gamebryo = None;
        let mut m = Manager::open(game, t.path().join("game"), t.path().join("mods")).unwrap();
        for (name, files) in [
            (
                "ModA",
                &[
                    ("textures/sky.dds", "A"),
                    ("textures/a.dds", "a"),
                    ("textures/patched.dds", "A"),
                ][..],
            ),
            (
                "ModB",
                &[("textures/same.dds", "same"), ("textures/gone.dds", "g")],
            ),
        ] {
            let z = t.path().join(format!("{name}.zip"));
            zip(&z, files);
            m.install(m.stage(&z).unwrap(), None).unwrap();
        }
        // ModB off: same.dds is the game's own file again, byte-identical to ModB's copy.
        m.set_active("ModB", false).unwrap();
        let backup = m.mods_dir.join("Backup/textures");
        fs::write(backup.join("gone.dds"), "game file").unwrap(); // stranded: target empty
        fs::remove_file(data.join("textures/patched.dds")).unwrap(); // game updated the file the mod displaced
        fs::write(data.join("textures/patched.dds"), "v2").unwrap();
        let want = m.state.deployed.clone();
        m.state.deployed.clear();
        m.save().unwrap();

        let mut m = Manager::open(m.game.clone(), m.game_path.clone(), m.mods_dir.clone()).unwrap();
        let report = m.repair().unwrap();
        assert_eq!(
            report.orphaned_backups,
            ["OrphanedBackup/Backup/textures/patched.dds"]
        );
        assert_eq!(report.untracked, ["Data/textures/same.dds"]);
        assert!(
            m.repair().unwrap().orphaned_backups.is_empty(),
            "only this run's are reported"
        );
        assert!(m.in_sync());
        assert_eq!(m.state.deployed, want);
        let read = |p: &Path| fs::read_to_string(p).unwrap();
        assert_eq!(read(&data.join("textures/sky.dds")), "A");
        assert_eq!(read(&backup.join("sky.dds")), "vanilla");
        assert_eq!(read(&data.join("textures/gone.dds")), "game file");
        assert_eq!(read(&data.join("textures/same.dds")), "same");
        assert_eq!(read(&data.join("textures/patched.dds")), "A");
        assert_eq!(
            read(&backup.join("patched.dds")),
            "v2",
            "updated game file is the backup now"
        );

        m.set_active("ModA", false).unwrap();
        assert_eq!(read(&data.join("textures/sky.dds")), "vanilla");
        assert_eq!(read(&data.join("textures/patched.dds")), "v2");
        assert!(!data.join("textures/a.dds").exists() && !backup.exists());
        assert!(data.join("textures/same.dds").exists() && data.join("textures/gone.dds").exists());
    }

    /// state.json and its backup both lost: the stored mods are offered back; adopting them
    /// re-enables only the one whose files are live, and deploys nothing for the other.
    #[test]
    fn adopt_rebuilds_lost_library() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        let data = game_path.join("Data");
        fs::create_dir_all(data.join("textures")).unwrap();
        fs::write(data.join("textures/sky.dds"), "vanilla").unwrap();
        let mut game = crate::game::find("SkyrimSE").unwrap();
        game.plugin.as_mut().unwrap().policy = None;
        game.gamebryo = None;
        let mut m = Manager::open(game, game_path.clone(), t.path().join("mods")).unwrap();
        for (name, files) in [
            (
                "ModB",
                &[("textures/sky.dds", "B"), ("textures/b.dds", "b")][..],
            ),
            // loader.exe is always copied (realFileRequired), so only proven live via its siblings.
            (
                "ModA",
                &[
                    ("x/loader.exe", "exe"),
                    ("x/readme.txt", "r"),
                    ("x/Data/Scripts/a.pex", "p"),
                    ("x/Data/textures/sky.dds", "A"),
                ],
            ),
        ] {
            let z = t.path().join(format!("{name}.zip"));
            zip(&z, files);
            m.install(m.stage(&z).unwrap(), None).unwrap();
        }
        m.set_active("ModB", false).unwrap();
        let (files, deployed) = (
            m.state
                .mods
                .iter()
                .map(|x| (x.files.clone(), x.root_files.clone()))
                .collect::<Vec<_>>(),
            m.state.deployed.clone(),
        );
        fs::remove_file(m.mods_dir.join("state.json")).unwrap();
        fs::remove_file(m.mods_dir.join("state.json.bak")).unwrap();

        let mut m = Manager::open(m.game.clone(), m.game_path.clone(), m.mods_dir.clone()).unwrap();
        assert!(m.state.mods.is_empty());
        assert_eq!(m.strays(), ["ModA", "ModB"]);
        assert_eq!(m.adopt().unwrap(), RepairReport::default());
        assert!(m.strays().is_empty() && m.in_sync());
        let got: Vec<_> = m
            .state
            .mods
            .iter()
            .map(|x| (x.id.as_str(), x.active, x.game_root))
            .collect();
        assert_eq!(got, [("ModA", true, true), ("ModB", false, false)]);
        let mut got_files: Vec<_> = m
            .state
            .mods
            .iter()
            .map(|x| (x.files.clone(), x.root_files.clone()))
            .collect();
        got_files.reverse();
        assert_eq!(got_files, files);
        assert_eq!(m.state.deployed, deployed);
        assert_eq!(
            fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
            "A"
        );
        assert!(!data.join("textures/b.dds").exists() && game_path.join("loader.exe").exists());

        m.set_active("ModA", false).unwrap();
        assert_eq!(
            fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
            "vanilla"
        );
        assert!(
            !game_path.join("loader.exe").exists() && !m.mods_dir.join("GameRootBackup").exists()
        );
        m.set_active("ModB", true).unwrap();
        assert_eq!(
            fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
            "B"
        );
    }

    #[test]
    fn script_extender_deploys_to_game_root() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        fs::create_dir_all(game_path.join("Data")).unwrap();
        fs::write(game_path.join("steam_api64.dll"), "vanilla").unwrap();
        let mut game = crate::game::find("SkyrimSE").unwrap();
        game.plugin.as_mut().unwrap().policy = None;
        game.gamebryo = None;
        let mut m = Manager::open(game, game_path.clone(), t.path().join("mods")).unwrap();

        let skse = t.path().join("skse64.zip");
        zip(
            &skse,
            &[
                ("skse64_2_02_06/skse64_loader.exe", "exe"),
                ("skse64_2_02_06/steam_api64.dll", "modded"),
                ("skse64_2_02_06/readme.txt", "r"),
                ("skse64_2_02_06/Data/Scripts/Test.pex", "pex"),
            ],
        );
        let s = m.stage(&skse).unwrap();
        m.install(s, None).unwrap();
        assert_eq!(
            fs::read_to_string(game_path.join("skse64_loader.exe")).unwrap(),
            "exe"
        );
        assert_eq!(
            fs::read_to_string(game_path.join("steam_api64.dll")).unwrap(),
            "modded"
        );
        assert_eq!(
            fs::read_to_string(game_path.join("Data/Scripts/Test.pex")).unwrap(),
            "pex"
        );
        assert!(!game_path.join("Data/skse64_loader.exe").exists());

        // Same name in Data from another mod is a different file, not a conflict.
        let other = t.path().join("other.zip");
        zip(
            &other,
            &[
                ("Data/skse64_loader.exe", "data"),
                ("Data/Scripts/Test.pex", "other"),
            ],
        );
        let s = m.stage(&other).unwrap();
        m.install(s, None).unwrap();
        assert_eq!(
            fs::read_to_string(game_path.join("skse64_loader.exe")).unwrap(),
            "exe"
        );
        assert_eq!(m.conflicts()["skse64"], vec!["other"]);

        // No binary beside Data: toggle defaults off, files kept for flipping it on later.
        let plain = t.path().join("plain.zip");
        zip(
            &plain,
            &[("wrap/plain.txt", "r"), ("wrap/Data/meshes/a.nif", "n")],
        );
        let s = m.stage(&plain).unwrap();
        m.install(s, None).unwrap();
        let p = m.state.mods.iter().find(|x| x.id == "plain").unwrap();
        assert!(!p.game_root && p.root_files == ["plain.txt"]);
        assert!(!game_path.join("plain.txt").exists());
        m.set_game_root("plain", true).unwrap();
        assert_eq!(
            fs::read_to_string(game_path.join("plain.txt")).unwrap(),
            "r"
        );
        m.set_game_root("plain", false).unwrap();
        assert!(!game_path.join("plain.txt").exists());

        // Toggling off withdraws root files and restores the displaced game file.
        m.set_game_root("skse64", false).unwrap();
        assert!(!game_path.join("skse64_loader.exe").exists());
        assert_eq!(
            fs::read_to_string(game_path.join("steam_api64.dll")).unwrap(),
            "vanilla"
        );
        assert_eq!(
            fs::read_to_string(game_path.join("Data/Scripts/Test.pex")).unwrap(),
            "other"
        );
        m.set_game_root("skse64", true).unwrap();

        let reopened =
            Manager::open(m.game.clone(), m.game_path.clone(), m.mods_dir.clone()).unwrap();
        assert!(reopened.state.deployed.contains_key("/skse64_loader.exe"));

        m.remove("skse64").unwrap();
        assert!(!game_path.join("skse64_loader.exe").exists());
        assert_eq!(
            fs::read_to_string(game_path.join("steam_api64.dll")).unwrap(),
            "vanilla"
        );
        assert!(!m.mods_dir.join("GameRootInstall/skse64").exists());
        assert!(!m.mods_dir.join("GameRootBackup/steam_api64.dll").exists());
    }

    #[test]
    fn loose_files_spare_base_deployed_and_creations() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        let data = game_path.join("Data");
        for f in [
            "Skyrim.esm",
            "Skyrim - Textures0.bsa",
            "loose.esp",
            "textures/old.dds",
            "ccBGSSSE001-Fish.esm",
            "ccBGSSSE001-Fish.bsa",
        ] {
            fs::create_dir_all(data.join(f).parent().unwrap()).unwrap();
            fs::write(data.join(f), "x").unwrap();
        }
        fs::write(game_path.join("Skyrim.ccc"), "ccBGSSSE001-Fish.esm\r\n").unwrap();
        let mut game = crate::game::find("SkyrimSE").unwrap();
        game.user_dir = Some(t.path().join("user"));
        let mut m = Manager::open(game, game_path.clone(), t.path().join("mods")).unwrap();
        let z = t.path().join("Mod.zip");
        zip(&z, &[("Data/mod.esp", "m")]);
        m.install(m.stage(&z).unwrap(), None).unwrap();
        assert_eq!(m.loose_files().unwrap(), ["loose.esp", "textures/old.dds"]);
        let to = m.purge_loose().unwrap().unwrap();
        assert!(to.join("textures/old.dds").exists() && !data.join("textures").exists());
        assert!(data.join("Skyrim.esm").exists() && data.join("mod.esp").exists());
        assert!(data.join("ccBGSSSE001-Fish.bsa").exists());
        assert_eq!(m.loose_files().unwrap(), Vec::<String>::new());
        // No base list: no loose-file view at all.
        let mut sdv = crate::game::find("stardewvalley").unwrap();
        sdv.user_dir = Some(t.path().join("user"));
        let m = Manager::open(sdv, game_path, t.path().join("mods2")).unwrap();
        assert_eq!(m.loose_files(), None);
    }

    #[test]
    fn game_specific_installs() {
        let t = tempfile::tempdir().unwrap();
        let user = t.path().join("user");
        let open = |mode: &str| {
            let mut g = crate::game::find(mode).unwrap();
            g.user_dir = Some(user.clone());
            let game_path = t.path().join(mode);
            fs::create_dir_all(&game_path).unwrap();
            Manager::open(g, game_path, t.path().join(format!("mods-{mode}"))).unwrap()
        };
        let docs = user.join("Documents");

        // Witcher 2: cook.hash mods are user content in Documents; others go to CookedPC.
        let mut m = open("Witcher2");
        let z = t.path().join("User.zip");
        zip(&z, &[("Cool Mod/cook.hash", "h"), ("Cool Mod/a.dzip", "a")]);
        m.install(m.stage(&z).unwrap(), None).unwrap();
        let w2 = docs.join("Witcher 2/UserContent/User");
        // The lone outer folder is the archive's wrapper: root files are named after the archive.
        assert_eq!(fs::read_to_string(w2.join("a.dzip")).unwrap(), "a");
        let z = t.path().join("Cooked.zip");
        zip(&z, &[("x.dzip", "x")]);
        m.install(m.stage(&z).unwrap(), None).unwrap();
        assert!(m.game_path.join("CookedPC/x.dzip").exists());
        m.remove("User").unwrap();
        assert!(!w2.exists());

        // Dragon Age .dazip: contents/ to Documents/BioWare/Dragon Age, manifest into AddIns.xml.
        let mut m = open("DragonAge");
        let settings = docs.join("BioWare/Dragon Age/Settings");
        fs::create_dir_all(&settings).unwrap();
        fs::write(settings.join("AddIns.xml"), "<AddInsList>\n</AddInsList>\n").unwrap();
        let z = t.path().join("Cool.dazip");
        zip(
            &z,
            &[
                ("Contents/addins/cool/core/a.erf", "e"),
                (
                    "Manifest.xml",
                    "<Manifest><AddInsList><AddInItem UID=\"cool\" /></AddInsList></Manifest>",
                ),
            ],
        );
        m.install(m.stage(&z).unwrap(), None).unwrap();
        let erf = docs.join("BioWare/Dragon Age/addins/cool/core/a.erf");
        assert!(erf.exists());
        assert!(!docs.join("BioWare/Dragon Age/Manifest.xml").exists());
        let addins = || fs::read_to_string(settings.join("AddIns.xml")).unwrap();
        assert!(addins().contains("UID=\"cool\""), "{}", addins());
        m.set_active("Cool", false).unwrap();
        assert!(!erf.exists() && !addins().contains("cool"));

        // X Rebirth: a root content.xml names the extension folder; removal asks first.
        let mut m = open("XRebirth");
        let z = t.path().join("xr.zip");
        zip(
            &z,
            &[
                ("content.xml", "<content name=\"Big\" />"),
                ("t/a.xml", "a"),
            ],
        );
        m.install(m.stage(&z).unwrap(), None).unwrap();
        assert!(m.game_path.join("extensions/Big/t/a.xml").exists());
        assert!(m.removal_warning("xr").unwrap().contains("Big mod"));

        // State of Decay: two content.xml is refused, nothing installed.
        let mut m = open("StateOfDecay");
        let z = t.path().join("two.zip");
        zip(
            &z,
            &[
                ("a/content.xml", "<c name=\"a\"/>"),
                ("b/content.xml", "<c name=\"b\"/>"),
            ],
        );
        let e = m.install(m.stage(&z).unwrap(), None).unwrap_err();
        assert!(e.to_string().contains("multiple mods"), "{e}");
        assert!(m.state.mods.is_empty());
    }

    #[test]
    fn path_adjustment_profile_applies_on_install() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        fs::create_dir_all(&game_path).unwrap();
        let mut m = Manager::open(
            crate::game::find("stardewvalley").unwrap(),
            game_path.clone(),
            t.path().join("mods"),
        )
        .unwrap();
        let z = t.path().join("Foo.zip");
        zip(&z, &[("Foo/manifest.json", "{}"), ("Foo/Foo.dll", "d")]);
        let s = m.stage(&z).unwrap();
        m.install(s, None).unwrap();
        assert_eq!(
            fs::read_to_string(game_path.join("Mods/Foo/manifest.json")).unwrap(),
            "{}"
        );
        assert!(game_path.join("Mods/Foo/Foo.dll").exists());
        m.remove("Foo").unwrap();
        assert!(!game_path.join("Mods").exists());

        // OBSE-style root files from a profile deploy outside Data and default the toggle on.
        let game_path = t.path().join("obr");
        fs::create_dir_all(game_path.join("OblivionRemastered/Content/Dev/ObvData/Data")).unwrap();
        let mut game = crate::game::find("OblivionRemastered").unwrap();
        game.plugin.as_mut().unwrap().policy = None;
        game.gamebryo = None;
        let mut m = Manager::open(game, game_path.clone(), t.path().join("mods-obr")).unwrap();
        let z = t.path().join("obse.zip");
        zip(&z, &[("OBSE/Plugins/x.dll", "x"), ("A.esp", "p")]);
        let s = m.stage(&z).unwrap();
        m.install(s, None).unwrap();
        assert!(m.state.mods[0].game_root);
        assert!(game_path
            .join("OblivionRemastered/Binaries/Win64/OBSE/Plugins/x.dll")
            .exists());
        assert!(game_path
            .join("OblivionRemastered/Content/Dev/ObvData/Data/A.esp")
            .exists());
    }

    #[test]
    fn nms_mod_folders_follow_load_order() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("nms");
        let settings = game_path.join("Binaries/SETTINGS/GCMODSETTINGS.MXML");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "cache").unwrap();
        let mut m = Manager::open(
            crate::game::find("NoMansSky").unwrap(),
            game_path.clone(),
            t.path().join("mods"),
        )
        .unwrap();
        for name in ["Alpha", "Beta"] {
            let z = t.path().join(format!("{name}.zip"));
            zip(&z, &[(&format!("{name}/MODELS/a.mbin"), name)]);
            let s = m.stage(&z).unwrap();
            m.install(s, None).unwrap();
        }
        let mods = game_path.join("GAMEDATA/MODS");
        assert_eq!(
            fs::read_to_string(mods.join("0000_Alpha/MODELS/a.mbin")).unwrap(),
            "Alpha"
        );
        assert!(mods.join("0001_Beta/MODELS/a.mbin").exists());
        assert!(!settings.exists());
        assert!(m.conflicts().is_empty());

        m.reorder("Beta", -1).unwrap();
        assert!(mods.join("0000_Beta/MODELS/a.mbin").exists());
        assert_eq!(
            fs::read_to_string(mods.join("0001_Alpha/MODELS/a.mbin")).unwrap(),
            "Alpha"
        );
        assert!(!mods.join("0000_Alpha").exists() && !mods.join("0001_Beta").exists());

        // Disabling a mod doesn't renumber the others.
        m.set_active("Beta", false).unwrap();
        assert!(mods.join("0001_Alpha/MODELS/a.mbin").exists());
        let reopened =
            Manager::open(m.game.clone(), m.game_path.clone(), m.mods_dir.clone()).unwrap();
        assert_eq!(reopened.state.deployed.len(), 1);
        m.remove("Alpha").unwrap();
        assert!(!mods.join("0001_Alpha").exists());
    }

    /// Needs a case-sensitive volume: Linux tmp, or on macOS point `NMM_TEST_TMPDIR` at a
    /// "Case-sensitive APFS" disk image. Skips itself on case-insensitive volumes.
    #[test]
    fn deploy_folds_into_existing_casing() {
        let t = match std::env::var_os("NMM_TEST_TMPDIR") {
            Some(d) => tempfile::tempdir_in(d).unwrap(),
            None => tempfile::tempdir().unwrap(),
        };
        fs::write(t.path().join("Probe"), "").unwrap();
        if t.path().join("probe").exists() {
            eprintln!("skipped: {} is case-insensitive", t.path().display());
            return;
        }
        let data = t.path().join("game/Data");
        fs::create_dir_all(data.join("textures")).unwrap();
        fs::write(data.join("textures/sky.dds"), "vanilla").unwrap();
        fs::write(data.join("Skyrim.esm"), "master").unwrap();
        let mut game = crate::game::find("SkyrimSE").unwrap();
        game.plugin.as_mut().unwrap().policy = None;
        game.gamebryo = None;
        let mut m = Manager::open(game, t.path().join("game"), t.path().join("mods")).unwrap();
        let names = |p: &Path| {
            let mut v: Vec<String> = fs::read_dir(p)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into())
                .collect();
            v.sort();
            v
        };

        let z = t.path().join("ModA.zip");
        zip(
            &z,
            &[
                ("Textures/SKY.dds", "A"),
                ("Textures/Sub/new.dds", "n"),
                ("skyrim.esm", "patched"),
            ],
        );
        let s = m.stage(&z).unwrap();
        m.install(s, None).unwrap();
        assert_eq!(names(&data), ["Skyrim.esm", "textures"]);
        assert_eq!(names(&data.join("textures")), ["Sub", "sky.dds"]);
        assert_eq!(
            fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
            "A"
        );
        assert_eq!(
            fs::read_to_string(data.join("Skyrim.esm")).unwrap(),
            "patched"
        );
        assert_eq!((m.fomod_env().file_state)("SKYRIM.ESM"), "Active");

        // A second mod with yet another casing joins the same tree.
        let z = t.path().join("ModB.zip");
        zip(&z, &[("TEXTURES/sub/other.dds", "o")]);
        let s = m.stage(&z).unwrap();
        m.install(s, None).unwrap();
        assert_eq!(names(&data.join("textures/Sub")), ["new.dds", "other.dds"]);

        m.remove("ModA").unwrap();
        m.remove("ModB").unwrap();
        assert_eq!(names(&data), ["Skyrim.esm", "textures"]);
        assert_eq!(names(&data.join("textures")), ["sky.dds"]);
        assert_eq!(
            fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
            "vanilla"
        );
        assert_eq!(
            fs::read_to_string(data.join("Skyrim.esm")).unwrap(),
            "master"
        );
        assert!(
            !m.mods_dir.join("Backup").exists() || names(&m.mods_dir.join("Backup")).is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn link_modes() {
        use std::os::unix::fs::MetadataExt;
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("game/Data");
        fs::create_dir_all(data.join("textures")).unwrap();
        fs::write(data.join("textures/sky.dds"), "vanilla").unwrap();
        let mut game = crate::game::find("SkyrimSE").unwrap();
        game.plugin.as_mut().unwrap().policy = None;
        game.gamebryo = None;
        game.mod_install.hardlink_required_extensions = vec![".ogg".into()];
        game.mod_install.real_file_required_extensions = vec![".dll".into()];
        let mut m = Manager::open(game, t.path().join("game"), t.path().join("mods")).unwrap();
        assert!(m.same_volume());
        let z = t.path().join("ModA.zip");
        zip(
            &z,
            &[
                ("textures/sky.dds", "A"),
                ("sound/a.ogg", "o"),
                ("a.dll", "d"),
            ],
        );
        let s = m.stage(&z).unwrap();
        m.install(s, None).unwrap();

        let ino = |p: &Path| fs::metadata(p).unwrap().ino();
        let is_link = |p: &str| fs::symlink_metadata(data.join(p)).unwrap().is_symlink();
        let hard = |m: &Manager, p: &str| {
            ino(&data.join(p)) == ino(&m.mods_dir.join("VirtualInstall/ModA").join(p))
        };

        // Auto: hardlinks, except real-copy files.
        assert!(hard(&m, "textures/sky.dds") && hard(&m, "sound/a.ogg") && !hard(&m, "a.dll"));

        m.set_link_mode(LinkMode::Symlink).unwrap();
        assert!(is_link("textures/sky.dds"));
        assert!(
            !is_link("sound/a.ogg") && hard(&m, "sound/a.ogg"),
            "hardlink-required ext stays a hardlink"
        );
        assert!(!is_link("a.dll") && !hard(&m, "a.dll"));
        assert_eq!(
            fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
            "A"
        );

        m.set_link_mode(LinkMode::Copy).unwrap();
        assert!(!is_link("textures/sky.dds") && !hard(&m, "textures/sky.dds"));
        assert_eq!(
            fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
            "A"
        );

        m.set_link_mode(LinkMode::Hardlink).unwrap();
        assert!(hard(&m, "textures/sky.dds"));
        let reopened =
            Manager::open(m.game.clone(), m.game_path.clone(), m.mods_dir.clone()).unwrap();
        assert_eq!(reopened.state.link_mode, LinkMode::Hardlink);

        // Symlinked mods still uninstall cleanly and give the game file back.
        m.set_link_mode(LinkMode::Symlink).unwrap();
        m.remove("ModA").unwrap();
        assert_eq!(
            fs::read_to_string(data.join("textures/sky.dds")).unwrap(),
            "vanilla"
        );
        assert!(!data.join("sound").exists() && !data.join("a.dll").exists());

        // Strict hardlink reports a failed link instead of quietly copying.
        let e = place_file(
            &t.path().join("missing"),
            &t.path().join("out"),
            LinkMode::Hardlink,
            false,
            false,
        )
        .unwrap_err();
        assert!(e.to_string().contains("hardlink mode"), "{e}");
    }

    /// Game on another volume (`NMM_TEST_OTHER_VOLUME`, e.g. a mounted disk image), mods in tmp.
    /// Skips without it.
    #[cfg(unix)]
    #[test]
    fn cross_volume_falls_back() {
        let Some(other) = std::env::var_os("NMM_TEST_OTHER_VOLUME") else {
            eprintln!("skipped: NMM_TEST_OTHER_VOLUME not set");
            return;
        };
        let (g, t) = (
            tempfile::tempdir_in(other).unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let data = g.path().join("Data");
        fs::create_dir_all(data.join("textures")).unwrap();
        fs::write(data.join("textures/sky.dds"), "vanilla").unwrap();
        let mut game = crate::game::find("SkyrimSE").unwrap();
        game.plugin.as_mut().unwrap().policy = None;
        game.gamebryo = None;
        let mut m = Manager::open(game, g.path().into(), t.path().join("mods")).unwrap();
        assert!(!m.same_volume());
        let z = t.path().join("ModA.zip");
        zip(&z, &[("textures/sky.dds", "A")]);
        let s = m.stage(&z).unwrap();
        m.install(s, None).unwrap();
        let sky = data.join("textures/sky.dds");
        assert_eq!(fs::read_to_string(&sky).unwrap(), "A");
        assert!(
            !fs::symlink_metadata(&sky).unwrap().is_symlink(),
            "auto copies across volumes"
        );

        m.set_link_mode(LinkMode::Symlink).unwrap();
        assert!(fs::symlink_metadata(&sky).unwrap().is_symlink());

        let e = m.set_link_mode(LinkMode::Hardlink).unwrap_err();
        assert!(e.to_string().contains("hardlink mode"), "{e}");
        // Failed deploy rolled back: game still has the mod file or the vanilla one, never nothing.
        assert!(fs::read_to_string(&sky).is_ok_and(|s| s == "A" || s == "vanilla"));

        m.set_link_mode(LinkMode::Auto).unwrap();
        m.remove("ModA").unwrap();
        assert_eq!(fs::read_to_string(&sky).unwrap(), "vanilla");
    }

    #[test]
    fn prefix_detection() {
        let t = tempfile::tempdir().unwrap();
        let game = crate::game::find("SkyrimSE").unwrap();
        fs::create_dir_all(t.path().join("wrap/inner/meshes")).unwrap();
        assert_eq!(find_prefix(t.path(), &game), t.path().join("wrap/inner"));
        fs::create_dir_all(t.path().join("wrap/other")).unwrap();
        assert_eq!(find_prefix(t.path(), &game), t.path());
    }

    #[test]
    fn starfield_deploy_enables_loose_files() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        fs::create_dir_all(game_path.join("Data")).unwrap();
        let mut game = crate::game::find("Starfield").unwrap();
        game.plugin.as_mut().unwrap().policy = None;
        let ini = t.path().join("My Games/StarfieldCustom.ini");
        game.gamebryo.as_mut().unwrap().user_game_data_path =
            Some(t.path().join("My Games").to_string_lossy().into());
        let mut m = Manager::open(game, game_path.clone(), t.path().join("mods")).unwrap();
        let z = t.path().join("A.zip");
        zip(&z, &[("Data/textures/a.dds", "A")]);
        let s = m.stage(&z).unwrap();
        m.install(s, None).unwrap();
        assert!(fs::read_to_string(&ini)
            .unwrap()
            .contains("sResourceDataDirsFinal="));
        // An INI that can't be written doesn't block deploys.
        fs::remove_file(&ini).unwrap();
        fs::create_dir(&ini).unwrap();
        m.set_active("A", false).unwrap();
        assert!(!game_path.join("Data/textures/a.dds").exists());
    }

    #[test]
    fn omod_stages_runs_script_and_installs() {
        use crate::omod::tests::{config, net_str, stream};
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        fs::create_dir_all(game_path.join("Data")).unwrap();
        fs::write(game_path.join("Data/Oblivion.esm"), "").unwrap();
        let txt = t
            .path()
            .join("appdata/plugins.txt")
            .to_string_lossy()
            .into_owned();
        let mut game = crate::game::find("Oblivion").unwrap();
        let gb = game.gamebryo.as_mut().unwrap();
        gb.user_game_data_path = Some(t.path().join("user").to_string_lossy().into());
        gb.plugins_file_path = Some(txt.clone());
        if let Some(p) = game.plugin.as_mut().and_then(|p| p.policy.as_mut()) {
            p.plugins_file_path = Some(txt);
        }
        let mut m = Manager::open(game, game_path.clone(), t.path().join("mods")).unwrap();

        let (pcrc, pdata) = stream(&[("Mine.esp", b"TES4")], 0);
        let (dcrc, ddata) = stream(&[("meshes\\a.nif", b"nif"), ("opt.txt", b"o")], 0);
        let mut script = vec![];
        net_str(
            &mut script,
            "If DialogYesNo \"Skip opt?\"\r\nDontInstallDataFile opt.txt\r\nEndIf",
        );
        let a = t.path().join("Packed.omod");
        let mut z = zip::ZipWriter::new(fs::File::create(&a).unwrap());
        for (n, b) in [
            ("config", config("Packed Mod", 0)),
            ("plugins.crc", pcrc),
            ("plugins", pdata),
            ("data.crc", dcrc),
            ("data", ddata),
            ("script", script),
            ("image", b"PNG".to_vec()),
        ] {
            z.start_file(n, zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(&b).unwrap();
        }
        z.finish().unwrap();

        let mut s = m.stage(&a).unwrap();
        assert_eq!(s.info.as_ref().unwrap().name, "Packed Mod");
        assert!(s.screenshot.is_some() && s.fomod.is_none());
        assert!(m.run_mod_script(&mut s, &[]).unwrap().is_some());
        let answers = [modscript::Answer::Bool(true)];
        assert_eq!(m.run_mod_script(&mut s, &answers).unwrap(), None);
        m.install(s, None).unwrap();

        let data = game_path.join("Data");
        assert_eq!(fs::read(data.join("Mine.esp")).unwrap(), b"TES4");
        assert_eq!(fs::read(data.join("meshes/a.nif")).unwrap(), b"nif");
        assert!(!data.join("opt.txt").exists() && !data.join("fomod").exists());
        let e = &m.state.mods[0];
        assert_eq!(
            (e.name.as_str(), e.screenshot.as_deref()),
            ("Packed Mod", Some("screenshot"))
        );

        // Other games refuse .omod.
        let mut other = crate::game::find("Fallout4").unwrap();
        other.plugin.as_mut().unwrap().policy = None;
        other.gamebryo = None;
        let m2 = Manager::open(other, game_path, t.path().join("mods2")).unwrap();
        assert!(m2.stage(&a).is_err());
    }
}
