//! Nexus metadata on installed mods: md5 identification and update checks (intent of C#
//! `ModUpdateCheckTask` + `AutoUpdater`).

use super::{Client, FileInfo, Period};
use crate::manager::ModEntry;
use crate::{Error, Result, Task};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NexusMeta {
    pub domain: String,
    pub mod_id: u64,
    pub file_id: u64,
    pub mod_name: String,
    /// The installed file's display name and version.
    pub file_name: String,
    pub version: String,
    pub author: String,
    pub category_id: Option<u64>,
    /// Newer file in this file's update chain, from the last check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<Update>,
    /// Unix seconds this mod was last checked for updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Update {
    pub file_id: u64,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CheckReport {
    /// Mods newly matched to Nexus by md5.
    pub identified: usize,
    /// Mods whose files were looked up this run.
    pub checked: usize,
    /// Names of mods with an update available (new or already known).
    pub updates: Vec<String>,
    /// Per-mod problems; a rate limit ends the run early.
    pub errors: Vec<String>,
}

const DAY: u64 = 86_400;

/// Metadata for `file` of `mod_id`.
pub fn meta(c: &Client, domain: &str, mod_id: u64, file: &FileInfo) -> Result<NexusMeta> {
    let m = c.mod_info(domain, mod_id)?;
    Ok(NexusMeta {
        domain: domain.into(),
        mod_id,
        file_id: file.file_id,
        mod_name: m.name,
        file_name: file.name.clone(),
        version: file.version.clone(),
        author: m.author,
        category_id: m.category_id,
        update: None,
        checked: None,
    })
}

/// Which Nexus file `archive` is, by md5.
pub fn identify(c: &Client, domain: &str, archive: &Path) -> Result<Option<NexusMeta>> {
    let md5 = crate::download::md5_file(archive)?;
    match c.by_md5(domain, &md5)?.into_iter().next() {
        Some(hit) => meta(c, domain, hit.mod_id, &hit.file).map(Some),
        None => Ok(None),
    }
}

/// Identify unknown mods (md5 of their archive), then look for newer files of known ones.
/// Mods never checked are always looked up; ones checked within four weeks only when Nexus lists
/// them as updated since. Stops early (keeping what it learnt) on a rate limit.
pub fn check(
    c: &Client,
    domain: &str,
    mods: &mut [ModEntry],
    now: u64,
    task: &Task,
) -> Result<CheckReport> {
    let mut r = CheckReport::default();
    let n = mods.len() as u64;
    let limited = |r: &mut CheckReport| r.errors.push(Error::RateLimited.to_string());
    'run: {
        for (i, m) in mods.iter_mut().enumerate() {
            task.step("Identifying mods on Nexus", i as u64, n)?;
            let archive = Path::new(&m.archive);
            if m.nexus.is_some() || m.archive.is_empty() || !archive.is_file() {
                continue;
            }
            match identify(c, domain, archive) {
                Ok(Some(meta)) => {
                    tracing::info!(mod_ = %m.name, mod_id = meta.mod_id, file_id = meta.file_id, "matched on Nexus by md5");
                    m.nexus = Some(meta);
                    r.identified += 1;
                }
                Ok(None) => {}
                Err(Error::RateLimited) => {
                    limited(&mut r);
                    break 'run;
                }
                Err(e) => r.errors.push(format!("{}: {e}", m.name)),
            }
        }
        let oldest = mods
            .iter()
            .filter_map(|m| m.nexus.as_ref().filter(|n| n.domain == domain)?.checked)
            .min();
        let recent: Option<HashSet<u64>> = match oldest.map(|l| now.saturating_sub(l)) {
            Some(age) if age < 28 * DAY => {
                let p = match age {
                    a if a < DAY => Period::Day,
                    a if a < 7 * DAY => Period::Week,
                    _ => Period::Month,
                };
                match c.updated(domain, p) {
                    Ok(ids) => Some(ids.into_iter().collect()),
                    Err(Error::RateLimited) => {
                        limited(&mut r);
                        break 'run;
                    }
                    Err(e) => {
                        r.errors.push(format!("updated mods list: {e}"));
                        None // check everything instead
                    }
                }
            }
            _ => None,
        };
        for (i, m) in mods.iter_mut().enumerate() {
            task.step("Checking for updates", i as u64, n)?;
            let Some(meta) = &mut m.nexus else { continue };
            let unchanged = recent.as_ref().is_some_and(|s| !s.contains(&meta.mod_id));
            if meta.domain == domain && meta.checked.is_some() && unchanged {
                meta.checked = Some(now); // Nexus says nothing changed since
                continue;
            }
            match c.latest(&meta.domain, meta.mod_id, meta.file_id) {
                Ok(f) => {
                    r.checked += 1;
                    meta.checked = Some(now);
                    meta.update = (f.file_id != meta.file_id).then_some(Update {
                        file_id: f.file_id,
                        name: f.name,
                        version: f.version,
                    });
                }
                Err(Error::RateLimited) => {
                    limited(&mut r);
                    break 'run;
                }
                Err(e) => r.errors.push(format!("{}: {e}", m.name)),
            }
        }
    }
    r.updates = mods
        .iter()
        .filter(|m| m.nexus.as_ref().is_some_and(|n| n.update.is_some()))
        .map(|m| m.name.clone())
        .collect();
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::fake::{json, serve};
    use crate::nexus::Api;

    fn entry(name: &str, archive: &Path) -> ModEntry {
        serde_json::from_value(serde_json::json!({
            "id": name, "name": name, "archive": archive.to_string_lossy(), "files": [], "active": true
        }))
        .unwrap()
    }

    #[test]
    fn identifies_by_md5_then_checks_only_recently_updated_mods() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("SkyUI.7z");
        std::fs::write(&archive, b"skyui bytes").unwrap();
        let md5 = format!("{:x}", md5::compute(b"skyui bytes"));
        let (base, log) = serve(move |r| {
            let p = r.path.split('?').next().unwrap();
            let d = "/v1/games/skyrimspecialedition";
            if p == format!("{d}/mods/md5_search/{md5}.json") {
                json(
                    200,
                    r#"[{"mod":{"mod_id":12604},"file_details":{"file_id":10,"name":"SkyUI","version":"6.0","category_name":"MAIN","file_name":"a.7z","size_in_bytes":11,"uploaded_timestamp":1}}]"#,
                )
            } else if p == format!("{d}/mods/12604.json") {
                json(
                    200,
                    r#"{"mod_id":12604,"game_id":1704,"name":"SkyUI","version":"6.1","author":"SkyUI Team","summary":"","category_id":42}"#,
                )
            } else if p == format!("{d}/mods/12604/files.json") {
                json(
                    200,
                    r#"{"files":[{"file_id":10,"name":"SkyUI","version":"6.0","category_name":"OLD_VERSION","uploaded_timestamp":1},{"file_id":11,"name":"SkyUI","version":"6.1","category_name":"MAIN","uploaded_timestamp":2}],"file_updates":[{"old_file_id":10,"new_file_id":11}]}"#,
                )
            } else if p == format!("{d}/mods/updated.json") {
                json(200, r#"[{"mod_id":999}]"#)
            } else {
                json(404, r#"{"message":"No"}"#)
            }
        });
        let c = Client::with_base("k", Api::Rest, &base);
        let missing = tmp.path().join("gone.7z");
        let mut mods = vec![entry("SkyUI", &archive), entry("Local", &missing)];

        let r = check(
            &c,
            "skyrimspecialedition",
            &mut mods,
            1_000_000,
            &Task::default(),
        )
        .unwrap();
        assert_eq!(
            (r.identified, r.checked, r.updates.clone()),
            (1, 1, vec!["SkyUI".to_string()])
        );
        let meta = mods[0].nexus.clone().unwrap();
        assert_eq!(meta.checked, Some(1_000_000));
        assert_eq!(
            (
                meta.mod_id,
                meta.file_id,
                meta.category_id,
                meta.author.as_str()
            ),
            (12604, 10, Some(42), "SkyUI Team")
        );
        assert_eq!(meta.update.unwrap().version, "6.1");
        assert!(mods[1].nexus.is_none());

        // An hour later: Nexus lists only mod 999 as updated today, so SkyUI isn't looked up again
        // (its known update stays flagged), but a mod installed meanwhile (never checked) is.
        let before = log.lock().unwrap().len();
        let mut fresh = mods[0].clone();
        fresh.name = "SkyUI again".into();
        fresh.nexus.as_mut().unwrap().checked = None;
        mods.push(fresh);
        let r = check(
            &c,
            "skyrimspecialedition",
            &mut mods,
            1_003_600,
            &Task::default(),
        )
        .unwrap();
        assert_eq!((r.checked, r.updates.len()), (1, 2));
        let new: Vec<String> = log.lock().unwrap()[before..]
            .iter()
            .map(|r| r.path.clone())
            .collect();
        assert_eq!(
            new,
            [
                "/v1/games/skyrimspecialedition/mods/updated.json?period=1d",
                "/v1/games/skyrimspecialedition/mods/12604/files.json"
            ]
        );
        assert_eq!(mods[0].nexus.as_ref().unwrap().checked, Some(1_003_600));
    }

    #[test]
    fn rate_limit_ends_the_check_early() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.7z");
        std::fs::write(&a, b"x").unwrap();
        let (base, log) = serve(|_| json(429, "{}"));
        let c = Client::with_base("k", Api::Rest, &base);
        let mut mods = vec![entry("A", &a), entry("B", &a)];
        let r = check(&c, "skyrim", &mut mods, 0, &Task::default()).unwrap();
        assert_eq!(r.errors, ["Nexus: rate limit reached, try again later"]);
        assert_eq!(log.lock().unwrap().len(), 1);
    }
}
