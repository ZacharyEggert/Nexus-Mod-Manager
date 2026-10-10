//! Mod library + deployment. Single orchestration point (see VirtualInstallArchitecture.md):
//! callers change intent (`active`, order, profiles) and call `deploy()`, which diffs desired
//! vs deployed links and applies the difference.
//!
//! Layout under `mods_dir`: `VirtualInstall/<id>/…` extracted mod files, `Backup/…` game files
//! displaced by mods, `.staging/` scratch, `state.json`.

use crate::fomod::{self, ModuleConfig, Selection};
use crate::game::GameDef;
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
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Deployed {
    pub path: String,
    pub mod_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Load order: later entries win file conflicts.
    pub mods: Vec<ModEntry>,
    /// Lower-cased relative path -> what is currently linked there.
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
        let state = match fs::read_to_string(mods_dir.join("state.json")) {
            Ok(s) => serde_json::from_str(&s).map_err(err)?,
            Err(_) => State::default(),
        };
        Ok(Self { game, game_path, mods_dir, state })
    }

    pub fn save(&self) -> R<()> {
        let tmp = self.mods_dir.join("state.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&self.state).map_err(err)?).map_err(err)?;
        fs::rename(tmp, self.mods_dir.join("state.json")).map_err(err)
    }

    fn virtual_dir(&self, id: &str) -> PathBuf {
        self.mods_dir.join("VirtualInstall").join(id)
    }

    pub fn install_root(&self) -> PathBuf {
        self.game.install_root(&self.game_path)
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
        // dest (lower) -> (source, dest); later inserts win, matching FOMod priority order.
        let mut map: BTreeMap<String, (PathBuf, String)> = BTreeMap::new();
        let mut put = |src: PathBuf, dest: String| {
            if !IGNORED.iter().any(|i| dest.rsplit('/').next().unwrap_or("").eq_ignore_ascii_case(i)) {
                map.insert(dest.to_lowercase(), (src, dest));
            }
        };
        let name;
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
                            put(file, if dest.is_empty() { r } else { format!("{dest}/{r}") });
                        }
                    } else {
                        let d = if dest.is_empty() || dest == "." { f.source.rsplit('/').next().unwrap().to_string() } else { dest.to_string() };
                        put(src.clone(), d);
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
                        put(file, r);
                    }
                }
            }
        }

        let vdir = self.virtual_dir(&staged.id);
        let _ = fs::remove_dir_all(&vdir);
        let mut files = vec![];
        for (src, dest) in map.into_values() {
            let to = vdir.join(&dest);
            fs::create_dir_all(to.parent().unwrap()).map_err(err)?;
            fs::copy(&src, &to).map_err(err)?;
            files.push(dest);
        }
        let _ = fs::remove_dir_all(&staged.dir);
        self.state.mods.push(ModEntry {
            id: staged.id,
            name,
            archive: staged.archive.to_string_lossy().into_owned(),
            files,
            active: true,
        });
        self.deploy()
    }

    pub fn remove(&mut self, id: &str) -> R<()> {
        self.state.mods.retain(|m| m.id != id);
        self.deploy()?;
        fs::remove_dir_all(self.virtual_dir(id)).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(err(e)) })
    }

    pub fn set_active(&mut self, id: &str, active: bool) -> R<()> {
        self.state.mods.iter_mut().filter(|m| m.id == id).for_each(|m| m.active = active);
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
            for f in &m.files {
                owners.entry(f.to_lowercase()).or_default().push(&m.id);
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

    pub fn deploy(&mut self) -> R<()> {
        let root = self.install_root();
        let backup = self.mods_dir.join("Backup");
        let mut desired: BTreeMap<String, Deployed> = BTreeMap::new();
        for m in self.state.mods.iter().filter(|m| m.active) {
            for f in &m.files {
                desired.insert(f.to_lowercase(), Deployed { path: f.clone(), mod_id: m.id.clone() });
            }
        }

        // ponytail: state is saved per phase, not per file; a crash mid-phase can orphan links.
        let stale: Vec<(String, Deployed)> =
            self.state.deployed.iter().filter(|(k, d)| desired.get(*k) != Some(d)).map(|(k, d)| (k.clone(), d.clone())).collect();
        for (key, d) in stale {
            let target = root.join(&d.path);
            match fs::remove_file(&target) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("remove {}: {e}", target.display())),
            }
            self.state.deployed.remove(&key);
            let saved = backup.join(&d.path);
            if !desired.contains_key(&key) && saved.exists() {
                move_file(&saved, &target)?;
                prune(saved.parent().unwrap(), &backup);
            } else {
                prune(target.parent().unwrap(), &root);
            }
        }
        self.save()?;

        let mut result = Ok(());
        for (key, d) in desired {
            if self.state.deployed.contains_key(&key) {
                continue;
            }
            let target = root.join(&d.path);
            let saved = backup.join(&d.path);
            if target.exists() && !saved.exists() {
                move_file(&target, &saved)?;
            } else if target.exists() {
                let _ = fs::remove_file(&target);
            }
            let src = self.virtual_dir(&d.mod_id).join(&d.path);
            if let Err(e) = link_or_copy(&src, &target, self.game.needs_real_copy(&d.path)) {
                result = Err(e);
                break;
            }
            self.state.deployed.insert(key, d);
        }
        self.sync_plugins()?;
        self.save()?;
        result
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
