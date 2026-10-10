//! LOOT auto-sort through libloot (replaces C# `AutoPluginSortingTask` + loot32/64.dll). The
//! masterlist and prelude come from the LOOT project's per-game GitHub repos and are cached in
//! `<mods dir>/loot/`; without them LOOT still sorts by masters and record overlaps.

use crate::plugins::Policy;
use crate::{write_durable, IoContext, Result};
use libloot::GameType;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Masterlist branch matching the libloot version (schema changes come with LOOT releases).
pub const BRANCH: &str = "v0.29";
const RAW: &str = "https://raw.githubusercontent.com/loot";
/// Masterlists are refreshed before a sort when older than this.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 3600);

/// libloot game type + LOOT masterlist repo for a mode.
fn game(mode: &str) -> Option<(GameType, &'static str)> {
    Some(match mode {
        "Oblivion" => (GameType::Oblivion, "oblivion"),
        "OblivionRemastered" => (GameType::OblivionRemastered, "oblivion-remastered"),
        "Fallout3" => (GameType::Fallout3, "fallout3"),
        "FalloutNV" => (GameType::FalloutNV, "falloutnv"),
        "Skyrim" => (GameType::Skyrim, "skyrim"),
        "SkyrimSE" | "SkyrimGOG" => (GameType::SkyrimSE, "skyrimse"),
        "SkyrimVR" => (GameType::SkyrimVR, "skyrimvr"),
        "Fallout4" => (GameType::Fallout4, "fallout4"),
        "Fallout4VR" => (GameType::Fallout4VR, "fallout4vr"),
        "Starfield" => (GameType::Starfield, "starfield"),
        "Morrowind" => (GameType::Morrowind, "morrowind"),
        "Enderal" | "EnderalSE" => (GameType::Skyrim, "enderal"),
        _ => return None,
    })
}

pub fn supported(mode: &str) -> bool {
    game(mode).is_some()
}

fn masterlist(dir: &Path) -> PathBuf {
    dir.join("masterlist.yaml")
}

/// Age of the cached masterlist; None when there is none.
pub fn masterlist_age(dir: &Path) -> Option<Duration> {
    let t = fs::metadata(masterlist(dir))
        .and_then(|m| m.modified())
        .ok()?;
    Some(SystemTime::now().duration_since(t).unwrap_or_default())
}

/// Fetch the game's masterlist + prelude into `dir`. True when either changed.
pub fn update_masterlist(mode: &str, dir: &Path) -> Result<bool> {
    let (_, repo) = game(mode).ok_or("LOOT doesn't support this game")?;
    update_from(
        &format!("{RAW}/{repo}/{BRANCH}"),
        &format!("{RAW}/prelude/{BRANCH}"),
        dir,
    )
}

fn update_from(game_base: &str, prelude_base: &str, dir: &Path) -> Result<bool> {
    let http = reqwest::blocking::Client::builder()
        .user_agent(concat!("NexusModManager/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    fs::create_dir_all(dir).at(dir)?;
    let mut changed = false;
    for (base, file) in [
        (game_base, "masterlist.yaml"),
        (prelude_base, "prelude.yaml"),
    ] {
        let url = format!("{base}/{file}");
        let resp = http.get(&url).send().map_err(|e| format!("{url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{url}: HTTP {}", resp.status().as_u16()).into());
        }
        let body = resp.bytes().map_err(|e| format!("{url}: {e}"))?;
        let path = dir.join(file);
        if fs::read(&path).ok().as_deref() != Some(&body[..]) {
            write_durable(&path, &body)?;
            changed = true;
        } else {
            // Unchanged: still mark it fresh so the age check doesn't refetch every sort.
            let _ = fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_modified(SystemTime::now()));
        }
    }
    Ok(changed)
}

/// LOOT's order for `names` (given in current load order), using the masterlist in `dir` if any.
/// Plugins libloot can't read keep their current position.
pub fn sort(
    mode: &str,
    policy: &Policy,
    game_path: &Path,
    dir: &Path,
    names: &[String],
) -> Result<Vec<String>> {
    let (kind, _) = game(mode).ok_or("LOOT doesn't support this game")?;
    let err = |e: &dyn std::fmt::Display| crate::Error::from(format!("LOOT: {e}"));
    let local = policy.plugins_file.parent().unwrap_or(game_path);
    let mut g = libloot::Game::with_local_path(kind, game_path, local).map_err(|e| err(&e))?;
    let ml = masterlist(dir);
    if ml.exists() {
        let db = g.database();
        let mut db = db.write().map_err(|e| err(&e))?;
        let prelude = dir.join("prelude.yaml");
        if prelude.exists() {
            db.load_masterlist_with_prelude(&ml, &prelude)
        } else {
            db.load_masterlist(&ml)
        }
        .map_err(|e| err(&e))?;
    }
    let (valid, invalid): (Vec<&String>, Vec<&String>) = names
        .iter()
        .partition(|n| g.is_valid_plugin(&policy.dir.join(n)));
    let paths: Vec<PathBuf> = valid.iter().map(|n| policy.dir.join(n)).collect();
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    g.load_plugins(&refs).map_err(|e| err(&e))?;
    let order: Vec<&str> = valid.iter().map(|n| n.as_str()).collect();
    let mut sorted = g.sort_plugins(&order).map_err(|e| err(&e))?;
    // ponytail: unreadable plugins go back at their old index; LOOT can't place what it can't parse.
    for n in invalid {
        let i = names.iter().position(|x| x == n).unwrap().min(sorted.len());
        sorted.insert(i, n.clone());
    }
    Ok(sorted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::fake;
    use crate::plugins::tests::tes4;

    #[test]
    fn masterlist_rules_and_masters_drive_the_sort() {
        let t = tempfile::tempdir().unwrap();
        let game_path = t.path().join("game");
        let data = game_path.join("Data");
        fs::create_dir_all(&data).unwrap();
        for (n, flags, masters) in [
            ("Skyrim.esm", 1, vec![]),
            ("B.esp", 0, vec!["Skyrim.esm"]),
            ("A.esp", 0, vec!["Skyrim.esm"]),
            ("Patch.esp", 0, vec!["Skyrim.esm", "Lib.esp"]),
            ("Lib.esp", 0, vec!["Skyrim.esm"]),
        ] {
            fs::write(data.join(n), tes4(flags, &masters, "")).unwrap();
        }
        fs::write(data.join("Junk.esp"), "not a plugin").unwrap();
        let mut policy = Policy::new(&crate::game::find("SkyrimSE").unwrap(), &game_path).unwrap();
        policy.plugins_file = t.path().join("local/plugins.txt");
        let dir = t.path().join("loot");
        fs::create_dir_all(&dir).unwrap();
        let names: Vec<String> = [
            "Skyrim.esm",
            "B.esp",
            "Junk.esp",
            "A.esp",
            "Patch.esp",
            "Lib.esp",
        ]
        .map(String::from)
        .to_vec();

        // No masterlist: masters only. Patch moves below Lib; Junk keeps its index.
        let sorted = sort("SkyrimSE", &policy, &game_path, &dir, &names).unwrap();
        assert_eq!(sorted[2], "Junk.esp");
        let pos = |s: &[String], n: &str| s.iter().position(|x| x == n).unwrap();
        assert!(
            pos(&sorted, "Lib.esp") < pos(&sorted, "Patch.esp"),
            "{sorted:?}"
        );

        // Masterlist rule: B after A.
        fs::write(
            masterlist(&dir),
            "plugins:\n  - name: 'B.esp'\n    after: [ 'A.esp' ]\n",
        )
        .unwrap();
        let sorted = sort("SkyrimSE", &policy, &game_path, &dir, &names).unwrap();
        assert!(pos(&sorted, "A.esp") < pos(&sorted, "B.esp"), "{sorted:?}");
        assert_eq!(sorted[0], "Skyrim.esm");
        assert!(!supported("stardewvalley"));
    }

    #[test]
    fn masterlist_update_downloads_and_reports_changes() {
        let (base, log) = fake::serve(|r| {
            let body = if r.path.ends_with("masterlist.yaml") {
                "plugins: []\n"
            } else {
                "common: []\n"
            };
            fake::json(if r.path.contains("missing") { 404 } else { 200 }, body)
        });
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("loot");
        assert_eq!(masterlist_age(&dir), None);
        let game = format!("{base}/skyrimse/{BRANCH}");
        let prelude = format!("{base}/prelude/{BRANCH}");
        assert!(update_from(&game, &prelude, &dir).unwrap());
        assert_eq!(
            fs::read_to_string(masterlist(&dir)).unwrap(),
            "plugins: []\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("prelude.yaml")).unwrap(),
            "common: []\n"
        );
        assert!(masterlist_age(&dir).unwrap() < Duration::from_secs(60));
        assert!(!update_from(&game, &prelude, &dir).unwrap());
        let paths: Vec<String> = log.lock().unwrap().iter().map(|r| r.path.clone()).collect();
        assert_eq!(paths[0], "/skyrimse/v0.29/masterlist.yaml");
        assert_eq!(paths[1], "/prelude/v0.29/prelude.yaml");
        // A failed fetch is an error and leaves the cached copy alone.
        let e = update_from(&format!("{base}/missing"), &prelude, &dir).unwrap_err();
        assert!(e.to_string().contains("HTTP 404"), "{e}");
        assert_eq!(
            fs::read_to_string(masterlist(&dir)).unwrap(),
            "plugins: []\n"
        );
    }
}
