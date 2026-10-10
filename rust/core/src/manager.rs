//! Mod library + deployment. Single orchestration point (see VirtualInstallArchitecture.md):
//! callers change intent (`active`, order, profiles) and call `deploy()`, which diffs desired
//! vs deployed links and applies the difference.
//!
//! Layout under `mods_dir`: `VirtualInstall/<id>/…` extracted mod files, `Backup/…` game files
//! displaced by mods, `GameRootInstall/<id>/…` + `GameRootBackup/…` the same for files deployed to
//! the game root instead of the install root (script extenders), `.staging/` scratch, `state.json`, and `deploy.journal` while a deploy
//! is in flight (reconciled against the filesystem on the next `open` if we died mid-deploy).

use crate::fomod::{self, ModuleConfig, Selection};
use crate::game::{has_ext, GameDef};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

type R<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

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
}

impl ModEntry {
    /// (path, is game-root) for every file this mod deploys right now.
    fn deployable(&self) -> impl Iterator<Item = (&String, bool)> {
        let root = if self.game_root { &self.root_files[..] } else { &[] };
        self.files.iter().map(|f| (f, false)).chain(root.iter().map(|f| (f, true)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Deployed {
    pub path: String,
    pub mod_id: String,
    /// `path` is relative to the game root, not the install root.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub game_root: bool,
}

/// `deployed` key. Game-root paths get a leading `/` so `x.dll` in the root and in Data don't collide.
fn key(path: &str, game_root: bool) -> String {
    let lower = path.to_lowercase();
    if game_root { format!("/{lower}") } else { lower }
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
}

/// Extracted archive waiting to be installed.
#[derive(Debug, Clone)]
pub struct Staged {
    pub id: String,
    pub archive: PathBuf,
    pub dir: PathBuf,
    /// `Some((base, config))` when the archive ships a FOMod installer.
    pub fomod: Option<(PathBuf, ModuleConfig)>,
}

pub struct Manager {
    pub game: GameDef,
    pub game_path: PathBuf,
    pub mods_dir: PathBuf,
    pub state: State,
}

const IGNORED: &[&str] = &["meta.ini"];
const IGNORED_DIRS: &[&str] = &["__MACOSX"];

fn rel(path: &Path, base: &Path) -> String {
    path.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/")
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| !IGNORED_DIRS.iter().any(|d| e.file_name().eq_ignore_ascii_case(d)))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| !IGNORED.iter().any(|i| e.file_name().eq_ignore_ascii_case(i)))
        .map(|e| e.into_path())
        .collect()
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
        let Ok(rd) = fs::read_dir(&cur) else { return dir.to_path_buf() };
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
        let is_stop = |n: &str| n.eq_ignore_ascii_case("fomod") || game.stop_folders.iter().any(|s| s.eq_ignore_ascii_case(n));
        if dirs.iter().any(|(n, _)| is_stop(n)) || files.iter().any(|f| game.is_plugin(f)) {
            return cur;
        }
        if let Some((_, p)) = dirs.iter().find(|(n, _)| plugin_dir.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(n))) {
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
    let is_data = |p: &Path| game.data_dir_name().zip(p.file_name()).is_some_and(|(d, n)| n.eq_ignore_ascii_case(d));
    let Some(base) = prefix.parent().filter(|_| game.mod_install.supports_game_root_install && prefix != dir && is_data(prefix)) else {
        return (vec![], false);
    };
    let files: Vec<_> = files_under(base).into_iter().filter(|f| !f.starts_with(prefix)).collect();
    let binary = files.iter().any(|f| has_ext(&f.to_string_lossy(), &[".exe".into(), ".dll".into(), ".asi".into()]));
    let files = files.into_iter().map(|f| (rel(&f, base), f)).filter(|(r, _)| !r.to_lowercase().starts_with("fomod/")).map(|(r, f)| (f, r)).collect();
    (files, binary)
}

fn link_or_copy(src: &Path, dst: &Path, real_copy: bool) -> R<()> {
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p).map_err(err)?;
    }
    // ponytail: hardlink -> copy; no symlink tier (Windows needs dev mode for it).
    if real_copy || fs::hard_link(src, dst).is_err() {
        fs::copy(src, dst).map_err(|e| format!("deploy {} -> {}: {e}", src.display(), dst.display()))?;
    }
    Ok(())
}

fn move_file(src: &Path, dst: &Path) -> R<()> {
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p).map_err(err)?;
    }
    if fs::rename(src, dst).is_err() {
        fs::copy(src, dst).map_err(err)?;
        fs::remove_file(src).map_err(err)?;
    }
    Ok(())
}

/// tmp + flush + rename, so `path` holds old or new bytes after a power cut, never garbage.
fn write_durable(path: &Path, bytes: &[u8]) -> R<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut f = fs::File::create(&tmp).map_err(err)?;
    f.write_all(bytes).map_err(err)?;
    f.sync_all().map_err(err)?;
    fs::rename(&tmp, path).map_err(err)?;
    // ponytail: dir flush is Unix-only; NTFS journals the rename itself.
    #[cfg(unix)]
    fs::File::open(path.parent().unwrap()).and_then(|d| d.sync_all()).map_err(err)?;
    Ok(())
}

/// Lower-cased path -> (deployed before, deployed after) for every path a deploy changes.
type Journal = BTreeMap<String, (Option<Deployed>, Option<Deployed>)>;

// ponytail: byte compare, not inode; a game file identical to a mod's reads as deployed.
fn same_content(a: &Path, b: &Path) -> bool {
    matches!((fs::metadata(a), fs::metadata(b)), (Ok(x), Ok(y)) if x.len() == y.len()) && fs::read(a).ok() == fs::read(b).ok()
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
        fs::create_dir_all(&mods_dir).map_err(err)?;
        let load = |f: &str| serde_json::from_str(&fs::read_to_string(mods_dir.join(f)).ok()?).ok();
        let state = match load("state.json").or_else(|| load("state.json.bak")) {
            Some(s) => s,
            None if mods_dir.join("state.json").exists() => return Err("state.json and its backup are unreadable".into()),
            None => State::default(),
        };
        let mut m = Self { game, game_path, mods_dir, state };
        if let Ok(s) = fs::read_to_string(m.journal_path()) {
            let journal = serde_json::from_str(&s).map_err(|e| format!("unreadable deploy.journal ({e}); delete it to continue"))?;
            m.reconcile(&journal)?;
            m.finish()?;
        }
        Ok(m)
    }

    pub fn save(&self) -> R<()> {
        let (path, bak) = (self.mods_dir.join("state.json"), self.mods_dir.join("state.json.bak"));
        // Previous state survives as .bak: a hardlink to the old, already-flushed inode, no copy.
        let _ = fs::remove_file(&bak);
        let _ = fs::hard_link(&path, &bak);
        write_durable(&path, &serde_json::to_vec_pretty(&self.state).map_err(err)?)
    }

    fn virtual_dir(&self, id: &str) -> PathBuf {
        self.mods_dir.join("VirtualInstall").join(id)
    }

    pub fn install_root(&self) -> PathBuf {
        self.game.install_root(&self.game_path)
    }

    fn place(&self, d: &Deployed) -> Place {
        let (root, backup, store) = if d.game_root {
            (self.game_path.clone(), self.mods_dir.join("GameRootBackup"), self.mods_dir.join("GameRootInstall").join(&d.mod_id))
        } else {
            (self.install_root(), self.mods_dir.join("Backup"), self.virtual_dir(&d.mod_id))
        };
        Place { target: root.join(&d.path), saved: backup.join(&d.path), src: store.join(&d.path), root, backup }
    }

    /// fileDependency state for FOMod conditions.
    pub fn file_state(&self, name: &str) -> String {
        // ponytail: no Inactive state; a present file counts as Active.
        if self.install_root().join(name.replace('\\', "/")).exists() { "Active" } else { "Missing" }.into()
    }

    // ---------- install ----------

    pub fn stage(&self, archive: &Path) -> R<Staged> {
        let stem = archive.file_stem().and_then(|s| s.to_str()).ok_or("bad archive name")?;
        let id: String = stem.chars().map(|c| if c.is_alphanumeric() || " -_.()".contains(c) { c } else { '_' }).collect();
        if self.state.mods.iter().any(|m| m.id == id) {
            return Err(format!("'{id}' is already installed; remove it first"));
        }
        let dir = self.mods_dir.join(".staging").join(&id);
        let _ = fs::remove_dir_all(&dir);
        crate::archive::extract(archive, &dir)?;
        let fomod = match fomod::locate(&dir) {
            Some((base, cfg)) => Some((base, fomod::parse(&fs::read(cfg).map_err(err)?)?)),
            None => None,
        };
        Ok(Staged { id, archive: archive.to_path_buf(), dir, fomod })
    }

    /// Install a staged archive. `sel` is required when the archive is a FOMod.
    pub fn install(&mut self, staged: Staged, sel: Option<&Selection>) -> R<()> {
        // key(dest) -> (source, dest, game_root); later inserts win, matching FOMod priority order.
        let mut map: BTreeMap<String, (PathBuf, String, bool)> = BTreeMap::new();
        let mut put = |src: PathBuf, dest: String, game_root: bool| {
            if !IGNORED.iter().any(|i| dest.rsplit('/').next().unwrap_or("").eq_ignore_ascii_case(i)) {
                map.insert(key(&dest, game_root), (src, dest, game_root));
            }
        };
        let name;
        let mut game_root = false;
        match (&staged.fomod, sel) {
            (Some((base, cfg)), Some(sel)) => {
                name = if cfg.name.is_empty() { staged.id.clone() } else { cfg.name.clone() };
                // Case-insensitive index of the archive: FOMod paths rarely match on-disk case.
                let index: HashMap<String, PathBuf> = walkdir::WalkDir::new(base)
                    .into_iter()
                    .filter_map(Result::ok)
                    .map(|e| (rel(e.path(), base).to_lowercase(), e.into_path()))
                    .collect();
                for f in cfg.files(sel, &|n| self.file_state(n)) {
                    let key = f.source.trim_matches('/').to_lowercase();
                    let src = index.get(&key).ok_or_else(|| format!("FOMod references missing file '{}'", f.source))?;
                    let dest = f.destination.trim_matches('/');
                    if f.is_folder {
                        for file in files_under(src) {
                            let r = rel(&file, src);
                            put(file, if dest.is_empty() { r } else { format!("{dest}/{r}") }, false);
                        }
                    } else {
                        let d = if dest.is_empty() || dest == "." { f.source.rsplit('/').next().unwrap().to_string() } else { dest.to_string() };
                        put(src.clone(), d, false);
                    }
                }
            }
            (Some(_), None) => return Err("FOMod selection required".into()),
            (None, _) => {
                name = staged.id.clone();
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

        let (mut files, mut root_files) = (vec![], vec![]);
        for (src, dest, game_root) in map.into_values() {
            let to = self.place(&Deployed { path: dest.clone(), mod_id: staged.id.clone(), game_root }).src;
            fs::create_dir_all(to.parent().unwrap()).map_err(err)?;
            fs::copy(&src, &to).map_err(err)?;
            if game_root { &mut root_files } else { &mut files }.push(dest);
        }
        let _ = fs::remove_dir_all(&staged.dir);
        self.state.mods.push(ModEntry {
            id: staged.id,
            name,
            archive: staged.archive.to_string_lossy().into_owned(),
            files,
            root_files,
            game_root,
            active: true,
        });
        self.deploy()
    }

    pub fn remove(&mut self, id: &str) -> R<()> {
        self.state.mods.retain(|m| m.id != id);
        self.deploy()?;
        for dir in [self.virtual_dir(id), self.mods_dir.join("GameRootInstall").join(id)] {
            match fs::remove_dir_all(dir) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(err(e)),
                _ => {}
            }
        }
        Ok(())
    }

    pub fn set_active(&mut self, id: &str, active: bool) -> R<()> {
        self.state.mods.iter_mut().filter(|m| m.id == id).for_each(|m| m.active = active);
        self.deploy()
    }

    pub fn set_game_root(&mut self, id: &str, on: bool) -> R<()> {
        self.state.mods.iter_mut().filter(|m| m.id == id).for_each(|m| m.game_root = on);
        self.deploy()
    }

    /// Move a mod `delta` places in load order (positive = later = higher priority).
    pub fn reorder(&mut self, id: &str, delta: isize) -> R<()> {
        let Some(i) = self.state.mods.iter().position(|m| m.id == id) else { return Ok(()) };
        let j = (i as isize + delta).clamp(0, self.state.mods.len() as isize - 1) as usize;
        let m = self.state.mods.remove(i);
        self.state.mods.insert(j, m);
        self.deploy()
    }

    // ---------- profiles ----------

    pub fn save_profile(&mut self, name: &str) -> R<()> {
        let active = self.state.mods.iter().filter(|m| m.active).map(|m| m.id.clone()).collect();
        self.state.profiles.insert(name.into(), active);
        self.save()
    }

    pub fn load_profile(&mut self, name: &str) -> R<()> {
        let ids = self.state.profiles.get(name).cloned().ok_or("no such profile")?;
        for m in &mut self.state.mods {
            m.active = ids.contains(&m.id);
        }
        self.deploy()
    }

    pub fn delete_profile(&mut self, name: &str) -> R<()> {
        self.state.profiles.remove(name);
        self.save()
    }

    /// Which mods each mod overrides / is overridden by, for the UI.
    pub fn conflicts(&self) -> BTreeMap<String, Vec<String>> {
        let mut owners: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for m in self.state.mods.iter().filter(|m| m.active) {
            for (f, game_root) in m.deployable() {
                owners.entry(key(f, game_root)).or_default().push(&m.id);
            }
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

    fn journal_path(&self) -> PathBuf {
        self.mods_dir.join("deploy.journal")
    }

    pub fn deploy(&mut self) -> R<()> {
        let mut desired: BTreeMap<String, Deployed> = BTreeMap::new();
        for m in self.state.mods.iter().filter(|m| m.active) {
            for (f, game_root) in m.deployable() {
                desired.insert(key(f, game_root), Deployed { path: f.clone(), mod_id: m.id.clone(), game_root });
            }
        }
        let deployed = &self.state.deployed;
        let journal: Journal = deployed
            .keys()
            .chain(desired.keys())
            .filter(|k| deployed.get(*k) != desired.get(*k))
            .map(|k| (k.clone(), (deployed.get(k).cloned(), desired.get(k).cloned())))
            .collect();

        // ponytail: link ops themselves aren't flushed; relies on APFS/NTFS/ext4 ordering metadata ops.
        if !journal.is_empty() {
            write_durable(&self.journal_path(), &serde_json::to_vec(&journal).map_err(err)?)?;
        }
        let result = self.apply(&journal);
        if result.is_err() {
            self.reconcile(&journal)?;
        }
        self.finish()?;
        result
    }

    /// Persist state after a deploy or recovery; only then is the journal obsolete.
    fn finish(&self) -> R<()> {
        self.sync_plugins()?;
        self.save()?;
        match fs::remove_file(self.journal_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(err(e)),
            _ => Ok(()),
        }
    }

    fn apply(&mut self, journal: &Journal) -> R<()> {
        for (key, (old, new)) in journal {
            let Some(d) = old else { continue };
            let Place { target, saved, root, backup, .. } = self.place(d);
            match fs::remove_file(&target) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("remove {}: {e}", target.display())),
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
            let Place { target, saved, src, .. } = self.place(d);
            if target.exists() && !saved.exists() {
                move_file(&target, &saved)?;
                crash_point();
            } else if target.exists() {
                let _ = fs::remove_file(&target);
            }
            link_or_copy(&src, &target, self.game.needs_real_copy(&d.path))?;
            crash_point();
            self.state.deployed.insert(key.clone(), d.clone());
        }
        Ok(())
    }

    /// Rebuild `deployed` for journaled paths from what is actually on disk, and put back
    /// game files stranded in Backup. Converges any partial `apply`; the next deploy redoes the rest.
    fn reconcile(&mut self, journal: &Journal) -> R<()> {
        for (key, (old, new)) in journal {
            let live = [new, old].into_iter().flatten().find(|d| {
                let p = self.place(d);
                same_content(&p.src, &p.target)
            });
            if let Some(d) = live {
                self.state.deployed.insert(key.clone(), d.clone());
                continue;
            }
            self.state.deployed.remove(key);
            let Place { target, saved, backup, .. } = self.place(new.as_ref().or(old.as_ref()).unwrap());
            if !saved.exists() {
                continue;
            }
            if !target.exists() {
                move_file(&saved, &target)?;
                prune(saved.parent().unwrap(), &backup);
            } else if same_content(&saved, &target) {
                // Crash inside move_file's copy fallback left two copies.
                fs::remove_file(&saved).map_err(err)?;
                prune(saved.parent().unwrap(), &backup);
            }
        }
        Ok(())
    }

    /// Rewrite our entries in plugins.txt; lines for plugins we don't manage are kept.
    fn sync_plugins(&self) -> R<()> {
        let Some(path) = self.game.plugins_file(&self.game_path) else { return Ok(()) };
        let marker = self.game.active_marker();
        let ours = |name: &str| self.state.mods.iter().flat_map(|m| &m.files).any(|f| f.eq_ignore_ascii_case(name));
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let mut lines: Vec<String> =
            existing.lines().filter(|l| !ours(l.trim().trim_start_matches('*'))).map(str::to_string).collect();
        // ponytail: managed plugins follow mod load order; per-plugin ordering UI not ported.
        for m in self.state.mods.iter().filter(|m| m.active) {
            for f in m.files.iter().filter(|f| !f.contains('/') && self.game.is_plugin(f)) {
                if self.state.deployed.get(&f.to_lowercase()).is_some_and(|d| d.mod_id == m.id) {
                    lines.push(format!("{marker}{f}"));
                }
            }
        }
        let body = lines.join("\r\n");
        if body.trim() == existing.trim() {
            return Ok(());
        }
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).map_err(err)?;
        }
        fs::write(&path, body + "\r\n").map_err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip(path: &Path, files: &[(&str, &str)]) {
        let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
        for (n, body) in files {
            z.start_file(*n, zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(body.as_bytes()).unwrap();
        }
        z.finish().unwrap();
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
        zip(&a, &[("ModA v1/textures/sky.dds", "A"), ("ModA v1/A.esp", "p"), ("ModA v1/meta.ini", "x")]);
        let b = t.path().join("ModB.zip");
        zip(&b, &[("Data/textures/sky.dds", "B")]);

        let s = m.stage(&a).unwrap();
        m.install(s, None).unwrap();
        assert_eq!(fs::read_to_string(data.join("textures/sky.dds")).unwrap(), "A");
        assert!(data.join("A.esp").exists() && !data.join("meta.ini").exists());

        let s = m.stage(&b).unwrap();
        m.install(s, None).unwrap();
        assert_eq!(fs::read_to_string(data.join("textures/sky.dds")).unwrap(), "B");
        assert_eq!(m.conflicts()["ModA"], vec!["ModB"]);

        m.reorder("ModB", -1).unwrap();
        assert_eq!(fs::read_to_string(data.join("textures/sky.dds")).unwrap(), "A");

        m.remove("ModA").unwrap();
        m.set_active("ModB", false).unwrap();
        assert_eq!(fs::read_to_string(data.join("textures/sky.dds")).unwrap(), "vanilla");
        assert!(!data.join("A.esp").exists());

        m.save_profile("b-on").unwrap();
        m.set_active("ModB", true).unwrap();
        m.load_profile("b-on").unwrap();
        assert_eq!(fs::read_to_string(data.join("textures/sky.dds")).unwrap(), "vanilla");

        let reopened = Manager::open(m.game.clone(), m.game_path.clone(), m.mods_dir.clone()).unwrap();
        assert_eq!(reopened.state.mods.len(), 1);

        fs::write(m.mods_dir.join("state.json"), "").unwrap(); // torn write after power cut
        let reopened = Manager::open(m.game.clone(), m.game_path.clone(), m.mods_dir.clone()).unwrap();
        assert_eq!(reopened.state.mods.len(), 1);
    }

    /// Kill deploy after every possible filesystem step; reopen must leave state matching disk
    /// and lose no game file, and a follow-up deploy must still converge.
    #[test]
    fn crash_mid_deploy_recovers() {
        // (active before, active after, expected sky.dds after)
        let cases: [(&[&str], &[&str], &str); 3] =
            [(&["ModA"], &["ModB"], "B"), (&["ModA"], &[], "vanilla"), (&[], &["ModA"], "A")];
        for (before, after, want) in cases {
            let mut crashes = 0;
            for n in 0.. {
                let t = tempfile::tempdir().unwrap();
                let data = t.path().join("game/Data");
                fs::create_dir_all(data.join("textures")).unwrap();
                fs::write(data.join("textures/sky.dds"), "vanilla").unwrap();
                let mut game = crate::game::find("Fallout4").unwrap();
                game.plugin.as_mut().unwrap().policy = None;
                game.gamebryo = None;
                let mut m = Manager::open(game, t.path().join("game"), t.path().join("mods")).unwrap();
                for (name, files) in [("ModA", &[("textures/sky.dds", "A"), ("A.esp", "p")][..]), ("ModB", &[("textures/sky.dds", "B")])] {
                    let z = t.path().join(format!("{name}.zip"));
                    zip(&z, files);
                    let s = m.stage(&z).unwrap();
                    m.install(s, None).unwrap();
                }
                let set = |m: &mut Manager, ids: &[&str]| m.state.mods.iter_mut().for_each(|x| x.active = ids.contains(&x.id.as_str()));
                set(&mut m, before);
                m.deploy().unwrap();

                set(&mut m, after);
                CRASH_AFTER.with(|c| c.set(n));
                let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| m.deploy())).is_err();
                CRASH_AFTER.with(|c| c.set(usize::MAX));
                let (game, game_path, mods_dir) = (m.game.clone(), m.game_path.clone(), m.mods_dir.clone());
                drop(m);

                let mut m = Manager::open(game, game_path, mods_dir).unwrap();
                assert!(!m.journal_path().exists());
                for d in m.state.deployed.values() {
                    assert!(same_content(&m.virtual_dir(&d.mod_id).join(&d.path), &data.join(&d.path)), "n={n} {d:?}");
                }
                let vanilla = [data.join("textures/sky.dds"), m.mods_dir.join("Backup/textures/sky.dds")]
                    .iter()
                    .filter(|p| fs::read_to_string(p).is_ok_and(|s| s == "vanilla"))
                    .count();
                assert_eq!(vanilla, 1, "n={n}: game file lost or duplicated");

                set(&mut m, after);
                m.deploy().unwrap();
                assert_eq!(fs::read_to_string(data.join("textures/sky.dds")).unwrap(), want, "n={n}");
                set(&mut m, &[]);
                m.deploy().unwrap();
                assert_eq!(fs::read_to_string(data.join("textures/sky.dds")).unwrap(), "vanilla", "n={n}");
                assert!(!data.join("A.esp").exists() && !m.mods_dir.join("Backup/textures/sky.dds").exists());

                if !crashed {
                    break;
                }
                crashes += 1;
            }
            assert!(crashes > 0);
        }
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
        zip(&skse, &[
            ("skse64_2_02_06/skse64_loader.exe", "exe"),
            ("skse64_2_02_06/steam_api64.dll", "modded"),
            ("skse64_2_02_06/readme.txt", "r"),
            ("skse64_2_02_06/Data/Scripts/Test.pex", "pex"),
        ]);
        let s = m.stage(&skse).unwrap();
        m.install(s, None).unwrap();
        assert_eq!(fs::read_to_string(game_path.join("skse64_loader.exe")).unwrap(), "exe");
        assert_eq!(fs::read_to_string(game_path.join("steam_api64.dll")).unwrap(), "modded");
        assert_eq!(fs::read_to_string(game_path.join("Data/Scripts/Test.pex")).unwrap(), "pex");
        assert!(!game_path.join("Data/skse64_loader.exe").exists());

        // Same name in Data from another mod is a different file, not a conflict.
        let other = t.path().join("other.zip");
        zip(&other, &[("Data/skse64_loader.exe", "data"), ("Data/Scripts/Test.pex", "other")]);
        let s = m.stage(&other).unwrap();
        m.install(s, None).unwrap();
        assert_eq!(fs::read_to_string(game_path.join("skse64_loader.exe")).unwrap(), "exe");
        assert_eq!(m.conflicts()["skse64"], vec!["other"]);

        // No binary beside Data: toggle defaults off, files kept for flipping it on later.
        let plain = t.path().join("plain.zip");
        zip(&plain, &[("wrap/plain.txt", "r"), ("wrap/Data/meshes/a.nif", "n")]);
        let s = m.stage(&plain).unwrap();
        m.install(s, None).unwrap();
        let p = m.state.mods.iter().find(|x| x.id == "plain").unwrap();
        assert!(!p.game_root && p.root_files == ["plain.txt"]);
        assert!(!game_path.join("plain.txt").exists());
        m.set_game_root("plain", true).unwrap();
        assert_eq!(fs::read_to_string(game_path.join("plain.txt")).unwrap(), "r");
        m.set_game_root("plain", false).unwrap();
        assert!(!game_path.join("plain.txt").exists());

        // Toggling off withdraws root files and restores the displaced game file.
        m.set_game_root("skse64", false).unwrap();
        assert!(!game_path.join("skse64_loader.exe").exists());
        assert_eq!(fs::read_to_string(game_path.join("steam_api64.dll")).unwrap(), "vanilla");
        assert_eq!(fs::read_to_string(game_path.join("Data/Scripts/Test.pex")).unwrap(), "other");
        m.set_game_root("skse64", true).unwrap();

        let reopened = Manager::open(m.game.clone(), m.game_path.clone(), m.mods_dir.clone()).unwrap();
        assert!(reopened.state.deployed.contains_key("/skse64_loader.exe"));

        m.remove("skse64").unwrap();
        assert!(!game_path.join("skse64_loader.exe").exists());
        assert_eq!(fs::read_to_string(game_path.join("steam_api64.dll")).unwrap(), "vanilla");
        assert!(!m.mods_dir.join("GameRootInstall/skse64").exists());
        assert!(!m.mods_dir.join("GameRootBackup/steam_api64.dll").exists());
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
}
