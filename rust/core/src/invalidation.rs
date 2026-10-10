//! Archive invalidation for Oblivion, Fallout 3 and New Vegas: makes the game load loose mod
//! files instead of the same files inside its BSAs. Port of C# `OblivionArchiveInvalidation`,
//! `Fallout3ArchiveInvalidation` and `FalloutNVArchiveInvalidation`.

use crate::game::GameDef;
use crate::{ini, IoContext, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    Oblivion,
    Fallout3,
    FalloutNV,
}

/// Empty BSA that redirects the game to loose files (C# payloads, byte for byte).
const OBLIVION_BSA: &[u8] = &[
    0x42, 0x53, 0x41, 0, 0x67, 0, 0, 0, 0x24, 0, 0, 0, 0x03, 0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0x02, 0, 0, 0,
];
const FALLOUT_BSA: &[u8] = &[
    0x42, 0x53, 0x41, 0, 0x67, 0, 0, 0, 0x24, 0, 0, 0, 0x03, 0x07, 0, 0, 0x01, 0, 0, 0, 0x01, 0, 0,
    0, 0x01, 0, 0, 0, 0x02, 0, 0, 0, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0, 0, 0, 0x36, 0,
    0, 0, 0x01, 0, 0x61, 0, 0x01, 0x61, 0, 0, 0, 0, 0, 0, 0, 0, 0x48, 0, 0, 0, 0x61, 0,
];

/// `day` of October `year`, midnight UTC: the dates the C# tool backdates vanilla BSAs to.
fn october(year: u64, day: u64) -> SystemTime {
    let days = match year {
        2005 => 13_057, // 2005-10-01
        _ => 14_153,    // 2008-10-01
    } + day
        - 1;
    SystemTime::UNIX_EPOCH + Duration::from_secs(days * 86_400)
}

pub struct ArchiveInvalidation {
    profile: Profile,
    data: PathBuf,
    game_path: PathBuf,
    ini: PathBuf,
    /// New Vegas also keeps the settings in `fallout_default.ini` when it exists.
    default_ini: Option<PathBuf>,
}

impl ArchiveInvalidation {
    /// None for games without an `archiveInvalidationProfile`.
    pub fn new(game: &GameDef, game_path: &Path) -> Option<Self> {
        let g = game.gamebryo.as_ref()?;
        let profile = match g
            .archive_invalidation_profile
            .as_deref()?
            .to_ascii_lowercase()
            .as_str()
        {
            "oblivion" => Profile::Oblivion,
            "fallout3" => Profile::Fallout3,
            "falloutnv" => Profile::FalloutNV,
            _ => return None,
        };
        Some(Self {
            profile,
            data: game.plugin_dir(game_path)?,
            game_path: game_path.to_path_buf(),
            ini: game.settings_path(g.ini_file_path.as_deref()?, game_path),
            default_ini: (profile == Profile::FalloutNV)
                .then(|| g.additional_settings_files.get("FODefaultIniPath"))
                .flatten()
                .map(|t| game.settings_path(t, game_path)),
        })
    }

    pub fn ini(&self) -> &Path {
        &self.ini
    }

    fn bsa(&self) -> &'static str {
        match self.profile {
            Profile::Oblivion => "BSARedirection.bsa",
            Profile::Fallout3 => "ArchiveInvalidationInvalidated!.bsa",
            Profile::FalloutNV => "Fallout - AI!.bsa",
        }
    }

    fn list(&self) -> Vec<String> {
        ini::get(&self.ini, "Archive", "SArchiveList")
            .unwrap_or_default()
            .split(',')
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .collect()
    }

    pub fn is_active(&self) -> bool {
        let older = || {
            ini::get(&self.ini, "Archive", "bInvalidateOlderFiles")
                .and_then(|v| v.parse::<i64>().ok())
                .is_some_and(|v| v != 0)
        };
        match self.profile {
            Profile::Oblivion => self.data.join(self.bsa()).exists(),
            Profile::Fallout3 => older(),
            Profile::FalloutNV => self.list().iter().any(|e| e == self.bsa()) || older(),
        }
    }

    /// `SArchiveList` without our BSA (and older tools' ones), with it first when `with_ours`.
    fn archive_list(&self, with_ours: bool) -> String {
        let drop: &[&str] = match self.profile {
            Profile::Oblivion => &[r"..\obmm\BSARedirection.bsa"],
            Profile::Fallout3 => &[],
            Profile::FalloutNV => &["ArchiveInvalidationInvalidated!.bsa"],
        };
        let mut out: Vec<String> = vec![];
        for e in self.list() {
            if e == self.bsa() || drop.contains(&e.as_str()) {
                continue;
            }
            // Old New Vegas behaviour, kept: Misc archives load first.
            if self.profile == Profile::FalloutNV && e.contains("Misc") {
                out.insert(0, e);
            } else {
                out.push(e);
            }
        }
        if with_ours {
            out.insert(0, self.bsa().into());
        }
        out.join(", ")
    }

    fn write(&self, section: &str, key: &str, value: &str) -> Result<()> {
        ini::set(&self.ini, section, key, value)?;
        if let Some(d) = self.default_ini.as_deref().filter(|d| d.exists()) {
            ini::set(d, section, key, value)?;
        }
        Ok(())
    }

    fn delete(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).at(path),
            _ => Ok(()),
        }
    }

    /// Backdate the game's own BSAs so loose files (newer) win: `Prefix*.bsa`, case-insensitive.
    fn backdate(&self, patterns: &[(&str, SystemTime)]) -> Result<()> {
        for e in fs::read_dir(&self.data)
            .at(&self.data)?
            .filter_map(|e| e.ok())
        {
            let name = e.file_name().to_string_lossy().to_lowercase();
            for (pat, time) in patterns {
                let (pre, post) = pat.to_lowercase().split_once('*').map_or_else(
                    || (pat.to_lowercase(), String::new()),
                    |(a, b)| (a.to_string(), b.to_string()),
                );
                let hit = if post.is_empty() && !pat.contains('*') {
                    name == pre
                } else {
                    name.starts_with(&pre)
                        && name.ends_with(&post)
                        && name.len() >= pre.len() + post.len()
                };
                if hit {
                    fs::File::options()
                        .write(true)
                        .open(e.path())
                        .and_then(|f| f.set_modified(*time))
                        .at(e.path())?;
                }
            }
        }
        Ok(())
    }

    /// Turn archive invalidation on or off. Needs the game's INI: the game creates it on first run.
    pub fn set(&self, on: bool) -> Result<()> {
        if !self.ini.exists() {
            return Err(format!(
                "{} not found. Run the game once so it creates its settings, then try again.",
                self.ini.display()
            )
            .into());
        }
        let bsa = self.data.join(self.bsa());
        let obmm = self.game_path.join("obmm").join("BSARedirection.bsa");
        let legacy_nv = self.data.join("ArchiveInvalidationInvalidated!.bsa");
        if on {
            match self.profile {
                Profile::Oblivion => {
                    let t = october(2005, 1);
                    self.backdate(&[("Oblivion - *.bsa", t), ("DLC*.bsa", t), ("Knights.bsa", t)])?;
                }
                Profile::Fallout3 => self.backdate(&[
                    ("Fallout - *.bsa", october(2008, 1)),
                    ("Anchorage - *.bsa", october(2008, 2)),
                    ("ThePitt - *.bsa", october(2008, 3)),
                    ("BrokenSteel - *.bsa", october(2008, 4)),
                    ("PointLookout - *.bsa", october(2008, 5)),
                    ("Zeta - *.bsa", october(2008, 6)),
                ])?,
                Profile::FalloutNV => self.backdate(&[
                    ("Fallout - *.bsa", october(2008, 1)),
                    ("ClassicPack - *.bsa", october(2008, 1)),
                ])?,
            }
            if self.profile != Profile::Oblivion {
                self.write("Archive", "bInvalidateOlderFiles", "1")?;
                self.write("General", "bLoadFaceGenHeadEGTFiles", "1")?;
            }
            self.write("Archive", "SInvalidationFile", "")?;
            Self::delete(&self.data.join("archiveinvalidation.txt"))?;
            match self.profile {
                Profile::Oblivion => Self::delete(&obmm)?,
                Profile::FalloutNV => Self::delete(&legacy_nv)?,
                Profile::Fallout3 => {}
            }
            let body = if self.profile == Profile::Oblivion {
                OBLIVION_BSA
            } else {
                FALLOUT_BSA
            };
            fs::write(&bsa, body).at(&bsa)?;
            self.write("Archive", "SArchiveList", &self.archive_list(true))
        } else {
            if self.profile != Profile::Oblivion {
                self.write("Archive", "bInvalidateOlderFiles", "0")?;
                self.write("General", "bLoadFaceGenHeadEGTFiles", "0")?;
            }
            self.write("Archive", "SInvalidationFile", "ArchiveInvalidation.txt")?;
            Self::delete(&bsa)?;
            match self.profile {
                Profile::Oblivion => Self::delete(&obmm)?,
                Profile::FalloutNV => Self::delete(&legacy_nv)?,
                Profile::Fallout3 => {}
            }
            self.write("Archive", "SArchiveList", &self.archive_list(false))
        }
    }
}

/// Starfield loads loose files only with `[Archive] bInvalidateOlderFiles=1` and an empty
/// `sResourceDataDirsFinal=` in StarfieldCustom.ini; old `sTestFile1..10` plugin entries must go.
/// Port of C# `DataDrivenGamebryoGameMode.EnsureStarfieldCustomIni`: creates the file when
/// missing, backs it up once to `<ini>.nmm_backup` before the first change. None if not Starfield.
pub fn starfield_loose_files(game: &GameDef, game_path: &Path) -> Option<Result<()>> {
    if game.mod_install.path_adjustment_profile != crate::adjust::Profile::Starfield {
        return None;
    }
    let ini = game.gamebryo.as_ref()?.ini_file_path.as_deref()?;
    Some(ensure_starfield_ini(&game.settings_path(ini, game_path)))
}

fn ensure_starfield_ini(path: &Path) -> Result<()> {
    let exists = path.exists();
    let old = if exists {
        ini::decode(&fs::read(path).at(path)?)
    } else {
        String::new()
    };
    let new = starfield_ini_text(&old);
    if exists && new == old {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).at(dir)?;
    }
    let backup = PathBuf::from(format!("{}.nmm_backup", path.display()));
    if exists && !backup.exists() {
        fs::copy(path, &backup).at(&backup)?;
    }
    crate::write_durable(path, &ini::encode(&new))
}

fn starfield_ini_text(text: &str) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let test_file = |l: &str| {
        ini::key_of(l).is_some_and(|k| {
            k.get(..9)
                .is_some_and(|p| p.eq_ignore_ascii_case("sTestFile"))
                && k[9..].parse::<u32>().is_ok_and(|n| (1..=10).contains(&n))
        })
    };
    let kept: Vec<&str> = text.lines().filter(|l| !test_file(l)).collect();
    let mut t = if kept.len() == text.lines().count() {
        text.to_string()
    } else {
        kept.join(nl) + nl
    };
    for (key, value) in [
        ("bInvalidateOlderFiles", "1"),
        ("sResourceDataDirsFinal", ""),
    ] {
        if ini::get_in(&t, "Archive", key).as_deref() != Some(value) {
            t = ini::set_in(&t, "Archive", key, value);
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Game + INI sandbox; the INI path is redirected into the temp dir.
    fn setup(mode: &str, bsas: &[&str], ini: &str) -> (tempfile::TempDir, ArchiveInvalidation) {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("game/Data");
        fs::create_dir_all(&data).unwrap();
        for b in bsas {
            fs::write(data.join(b), "").unwrap();
        }
        let mut game = crate::game::find(mode).unwrap();
        let g = game.gamebryo.as_mut().unwrap();
        g.user_game_data_path = Some(t.path().join("mygames").to_string_lossy().into());
        fs::create_dir_all(t.path().join("mygames")).unwrap();
        let name = g
            .ini_file_path
            .as_deref()
            .unwrap()
            .rsplit('\\')
            .next()
            .unwrap()
            .to_string();
        fs::write(t.path().join("mygames").join(name), ini).unwrap();
        let ai = ArchiveInvalidation::new(&game, &t.path().join("game")).unwrap();
        (t, ai)
    }

    fn mtime(p: &Path) -> SystemTime {
        p.metadata().unwrap().modified().unwrap()
    }

    #[test]
    fn oblivion_redirection_bsa() {
        let ini = "[Archive]\r\nSArchiveList=..\\obmm\\BSARedirection.bsa, Oblivion - Meshes.bsa\r\nSInvalidationFile=ArchiveInvalidation.txt\r\n";
        let (t, ai) = setup(
            "Oblivion",
            &["Oblivion - Meshes.bsa", "Knights.bsa", "Mod.bsa"],
            ini,
        );
        let data = t.path().join("game/Data");
        fs::write(data.join("ArchiveInvalidation.txt"), "").unwrap();
        assert!(!ai.is_active());
        ai.set(true).unwrap();
        assert!(ai.is_active());
        assert_eq!(
            fs::read(data.join("BSARedirection.bsa")).unwrap(),
            OBLIVION_BSA
        );
        assert!(!data.join("ArchiveInvalidation.txt").exists());
        assert_eq!(mtime(&data.join("Oblivion - Meshes.bsa")), october(2005, 1));
        assert_eq!(mtime(&data.join("Knights.bsa")), october(2005, 1));
        assert_ne!(mtime(&data.join("Mod.bsa")), october(2005, 1));
        assert_eq!(
            fs::read_to_string(ai.ini()).unwrap(),
            "[Archive]\r\nSArchiveList=BSARedirection.bsa, Oblivion - Meshes.bsa\r\nSInvalidationFile=\r\n"
        );
        ai.set(false).unwrap();
        assert!(!ai.is_active() && !data.join("BSARedirection.bsa").exists());
        assert_eq!(
            fs::read_to_string(ai.ini()).unwrap(),
            "[Archive]\r\nSArchiveList=Oblivion - Meshes.bsa\r\nSInvalidationFile=ArchiveInvalidation.txt\r\n"
        );
    }

    #[test]
    fn fallout3_ini_flags() {
        let ini = "[General]\r\nsLanguage=ENGLISH\r\n[Archive]\r\nSArchiveList=Fallout - Textures.bsa, Fallout - Meshes.bsa\r\n";
        let (t, ai) = setup(
            "Fallout3",
            &["Fallout - Textures.bsa", "Zeta - Main.bsa"],
            ini,
        );
        let data = t.path().join("game/Data");
        ai.set(true).unwrap();
        assert!(ai.is_active());
        assert_eq!(
            fs::read(data.join("ArchiveInvalidationInvalidated!.bsa")).unwrap(),
            FALLOUT_BSA
        );
        assert_eq!(mtime(&data.join("Zeta - Main.bsa")), october(2008, 6));
        let i = ai.ini();
        assert_eq!(
            ini::get(i, "Archive", "bInvalidateOlderFiles").as_deref(),
            Some("1")
        );
        assert_eq!(
            ini::get(i, "General", "bLoadFaceGenHeadEGTFiles").as_deref(),
            Some("1")
        );
        assert_eq!(
            ini::get(i, "Archive", "SArchiveList").as_deref(),
            Some(
                "ArchiveInvalidationInvalidated!.bsa, Fallout - Textures.bsa, Fallout - Meshes.bsa"
            )
        );
        ai.set(false).unwrap();
        assert!(!ai.is_active());
        assert_eq!(
            ini::get(i, "Archive", "SArchiveList").as_deref(),
            Some("Fallout - Textures.bsa, Fallout - Meshes.bsa")
        );
        assert_eq!(
            ini::get(i, "Archive", "SInvalidationFile").as_deref(),
            Some("ArchiveInvalidation.txt")
        );
    }

    #[test]
    fn new_vegas_default_ini_and_misc_first() {
        let ini = "[Archive]\r\nSArchiveList=ArchiveInvalidationInvalidated!.bsa, Fallout - Textures.bsa, Fallout - Misc.bsa\r\n";
        let (t, ai) = setup(
            "FalloutNV",
            &[
                "Fallout - Textures.bsa",
                "ArchiveInvalidationInvalidated!.bsa",
            ],
            ini,
        );
        let data = t.path().join("game/Data");
        let default = t.path().join("game/fallout_default.ini");
        fs::write(&default, "[Archive]\r\nbInvalidateOlderFiles=0\r\n").unwrap();
        ai.set(true).unwrap();
        assert!(ai.is_active());
        assert!(!data.join("ArchiveInvalidationInvalidated!.bsa").exists());
        assert!(data.join("Fallout - AI!.bsa").exists());
        assert_eq!(
            ini::get(ai.ini(), "Archive", "SArchiveList").as_deref(),
            Some("Fallout - AI!.bsa, Fallout - Misc.bsa, Fallout - Textures.bsa")
        );
        assert_eq!(
            ini::get(&default, "Archive", "bInvalidateOlderFiles").as_deref(),
            Some("1")
        );
        ai.set(false).unwrap();
        assert!(!ai.is_active());
        assert_eq!(
            ini::get(&default, "Archive", "bInvalidateOlderFiles").as_deref(),
            Some("0")
        );
    }

    #[test]
    fn needs_the_ini_and_a_profile() {
        let (t, ai) = setup("Oblivion", &[], "");
        fs::remove_file(ai.ini()).unwrap();
        let e = ai.set(true).unwrap_err().to_string();
        assert!(e.contains("Run the game once"), "{e}");
        assert!(
            ArchiveInvalidation::new(&crate::game::find("SkyrimSE").unwrap(), t.path()).is_none()
        );
    }

    #[test]
    fn starfield_ini_enables_loose_files() {
        // Missing file: created with just the [Archive] settings.
        assert_eq!(
            starfield_ini_text(""),
            "[Archive]\nbInvalidateOlderFiles=1\nsResourceDataDirsFinal=\n"
        );
        // Existing: wrong values replaced in place, sTestFile1..10 dropped (not 11, not comments).
        let text = "[General]\r\nsTestFile1=a.esp\r\nSTESTFILE10=b.esp\r\nsTestFile11=c.esp\r\n; sTestFile2=d\r\n\r\n[archive]\r\nbInvalidateOlderFiles=0\r\nsResourceDataDirsFinal=STRINGS\\\r\n";
        assert_eq!(
            starfield_ini_text(text),
            "[General]\r\nsTestFile11=c.esp\r\n; sTestFile2=d\r\n\r\n[archive]\r\nbInvalidateOlderFiles=1\r\nsResourceDataDirsFinal=\r\n"
        );
        // Already right: untouched.
        let ok = "[Archive]\r\nbInvalidateOlderFiles=1\r\nsResourceDataDirsFinal=\r\n";
        assert_eq!(starfield_ini_text(ok), ok);
    }

    #[test]
    fn starfield_ini_file_backup_once() {
        let t = tempfile::tempdir().unwrap();
        let mut game = crate::game::find("Starfield").unwrap();
        let g = game.gamebryo.as_mut().unwrap();
        g.user_game_data_path = Some(t.path().join("My Games/Starfield").to_string_lossy().into());
        let ini = t.path().join("My Games/Starfield/StarfieldCustom.ini");
        let bak = t
            .path()
            .join("My Games/Starfield/StarfieldCustom.ini.nmm_backup");
        // Missing folder + file: created, nothing to back up.
        starfield_loose_files(&game, t.path()).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(&ini).unwrap(),
            "[Archive]\nbInvalidateOlderFiles=1\nsResourceDataDirsFinal=\n"
        );
        assert!(!bak.exists());
        // User breaks it: fixed, original kept as backup; a second fix keeps the first backup.
        fs::write(&ini, "[Archive]\nbInvalidateOlderFiles=0\n").unwrap();
        starfield_loose_files(&game, t.path()).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(&bak).unwrap(),
            "[Archive]\nbInvalidateOlderFiles=0\n"
        );
        fs::write(&ini, "[Archive]\nsTestFile1=x.esp\n").unwrap();
        starfield_loose_files(&game, t.path()).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(&bak).unwrap(),
            "[Archive]\nbInvalidateOlderFiles=0\n"
        );
        assert_eq!(
            fs::read_to_string(&ini).unwrap(),
            "[Archive]\nbInvalidateOlderFiles=1\nsResourceDataDirsFinal=\n"
        );
        // Other games: no-op.
        assert!(starfield_loose_files(&crate::game::find("Fallout4").unwrap(), t.path()).is_none());
    }
}
