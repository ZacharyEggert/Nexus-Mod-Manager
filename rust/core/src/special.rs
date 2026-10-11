//! Game-specific bookkeeping outside the deployed files, ported from the C# game modes'
//! `SpecialFileInstall` / `ModFileMerge` / `CheckSecondaryUninstall`. C# edited these files once
//! per install/uninstall; here they're synced to the active mods after every deploy, so toggling
//! a mod or a profile switch keeps them right too. Only entries this manager added are removed.
//!
//! - Baldur's Gate 3: each active mod's `info.json` mods are listed in every
//!   `PlayerProfiles/*/modsettings.lsx` (except `default`).
//! - Dragon Age: active `.dazip` mods' `Manifest.xml` `AddInItem`s are in `Settings/AddIns.xml`.
//! - Dragon Age / Dragon Age 2: active mods' `chargenmorphcfg.xml` are merged into
//!   `<Override>/NMM_chargenmorphcfg/chargenmorphcfg.xml`.
//! - State of Decay / X Rebirth: removing a mod whose `content.xml` records it in saves warns.

use crate::adjust::{self, Profile};
use crate::game::GameDef;
use crate::manager::ModEntry;
use crate::{write_durable, IoContext, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const INFO: &str = "info.json";
const MANIFEST: &str = "Manifest.xml";
const CONTENT: &str = "content.xml";
const MORPH: &str = "chargenmorphcfg.xml";
const BG3_KEY: &str = "bg3";
const ADDINS_KEY: &str = "addins";

/// Files to keep from the archive for later bookkeeping (`ModEntry::special`). `read` gets a file's
/// bytes by its pre-adjustment path.
pub fn capture(
    game: &GameDef,
    m: &adjust::Mod,
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> BTreeMap<String, String> {
    let named = |n: &str| {
        m.files
            .iter()
            .find(|f| f.rsplit('/').next().unwrap_or(f).eq_ignore_ascii_case(n))
    };
    let want: &[&str] = match game.mod_install.path_adjustment_profile {
        Profile::BaldursGate3 => &[INFO],
        Profile::DragonAge if m.archive.to_ascii_lowercase().ends_with(".dazip") => &[MANIFEST],
        Profile::StateOfDecay | Profile::XRebirth => &[CONTENT],
        _ => &[],
    };
    want.iter()
        .filter_map(|n| {
            let bytes = read(named(n)?)?;
            Some((n.to_string(), String::from_utf8_lossy(&bytes).into_owned()))
        })
        .collect()
}

/// Bring the game's side files in line with `mods` (load order, active flags). `applied` is what
/// earlier syncs added, per file kind; it's updated in place. `store` maps (mod id, file) to the
/// mod's stored copy. Best effort: problems are logged, never fail a deploy (as in C#).
pub fn sync(
    game: &GameDef,
    game_path: &Path,
    mods: &[ModEntry],
    store: &dyn Fn(&str, &str) -> PathBuf,
    applied: &mut BTreeMap<String, Vec<String>>,
) {
    let active: Vec<&ModEntry> = mods.iter().filter(|m| m.active).collect();
    let mut run = |key: &str, f: &dyn Fn(&BTreeSet<String>) -> Result<Vec<String>>| {
        let before: BTreeSet<String> = applied.get(key).into_iter().flatten().cloned().collect();
        match f(&before) {
            Ok(now) if now.is_empty() => {
                applied.remove(key);
            }
            Ok(now) => {
                applied.insert(key.into(), now);
            }
            Err(e) => tracing::warn!(game = %game.mode_id, "{key} not updated: {e}"),
        }
    };
    match game.mod_install.path_adjustment_profile {
        Profile::BaldursGate3 => {
            let dir = game.path(
                "{LocalApplicationData}\\Larian Studios\\Baldur's Gate 3\\PlayerProfiles",
                game_path,
            );
            run(BG3_KEY, &|before| bg3_sync(&dir, &active, before));
        }
        Profile::DragonAge => {
            let file = game.path(
                "{PersonalData}\\BioWare\\Dragon Age\\Settings\\AddIns.xml",
                game_path,
            );
            run(ADDINS_KEY, &|before| addins_sync(&file, &active, before));
        }
        _ => {}
    }
    if matches!(game.mode_id.as_str(), "DragonAge" | "DragonAge2") {
        let out = game
            .install_root(game_path)
            .join("NMM_chargenmorphcfg")
            .join(MORPH);
        let sources: Vec<PathBuf> = active
            .iter()
            .flat_map(|m| {
                m.files
                    .iter()
                    .filter(|f| f.to_ascii_lowercase().ends_with(MORPH))
                    .map(|f| store(&m.id, f))
            })
            .collect();
        if let Err(e) = morph_sync(&out, &sources) {
            tracing::warn!(game = %game.mode_id, "{MORPH} not merged: {e}");
        }
    }
}

/// The C# "recorded in your save games" question for removing `m`, when it applies.
pub fn removal_warning(m: &ModEntry) -> Option<String> {
    let xml = m.special.get(CONTENT)?;
    let doc = roxmltree::Document::parse(xml.trim_start_matches('\u{feff}')).ok()?;
    let root = doc.root_element();
    let save = root.attribute("save").unwrap_or("");
    if !(save.is_empty() || save == "true" || save == "1") {
        return None;
    }
    let name = root.attribute("name").unwrap_or(&m.name);
    Some(format!(
        "The author of the {name} mod has chosen that their mod should get recorded in your save games.\n\
         If you played the game while this mod was active, your save files may no longer work.\n\
         Do you want to continue?"
    ))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Leading whitespace of the line `pos` is on, for indenting inserted nodes like their siblings.
fn indent_at(text: &str, pos: usize) -> &str {
    let start = text[..pos].rfind('\n').map_or(0, |i| i + 1);
    let line = &text[start..];
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// Remove byte ranges (with the whitespace before each) and insert `add` before `at`.
fn splice(text: &str, mut cut: Vec<std::ops::Range<usize>>, at: usize, add: &str) -> String {
    cut.sort_by_key(|r| r.start);
    let mut out = String::new();
    let mut pos = 0;
    for r in cut {
        let start = text[..r.start]
            .trim_end_matches([' ', '\t', '\r', '\n'])
            .len();
        if at >= pos && at <= start {
            out.push_str(&text[pos..at]);
            out.push_str(add);
            pos = at;
        }
        out.push_str(&text[pos..start]);
        pos = r.end;
    }
    if at >= pos {
        out.push_str(&text[pos..at]);
        out.push_str(add);
        pos = at;
    }
    out.push_str(&text[pos..]);
    out
}

fn read_text(path: &Path) -> Result<String> {
    let bytes = fs::read(path).at(path)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn parse(text: &str) -> Result<roxmltree::Document<'_>> {
    roxmltree::Document::parse(text.trim_start_matches('\u{feff}'))
        .map_err(|e| e.to_string().into())
}

/// Byte offset fix-up for a BOM `parse` skipped.
fn bom(text: &str) -> usize {
    text.len() - text.trim_start_matches('\u{feff}').len()
}

// ---------- Baldur's Gate 3 ----------

struct Bg3Mod {
    folder: String,
    name: String,
    uuid: String,
    version: String,
}

fn bg3_mods(info: &str) -> Vec<Bg3Mod> {
    let v: serde_json::Value =
        serde_json::from_str(info.trim_start_matches('\u{feff}')).unwrap_or_default();
    let s = |m: &serde_json::Value, keys: &[&str]| {
        keys.iter()
            .find_map(|k| m.get(*k).and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string()
    };
    v.get("Mods")
        .and_then(|m| m.as_array())
        .into_iter()
        .flatten()
        .map(|m| Bg3Mod {
            folder: s(m, &["Folder", "folderName"]),
            name: s(m, &["Name", "modName"]),
            uuid: s(m, &["UUID"]),
            version: Some(s(m, &["Version"]))
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "1".into()),
        })
        .filter(|m| !m.uuid.is_empty())
        .collect()
}

fn bg3_sync(dir: &Path, active: &[&ModEntry], before: &BTreeSet<String>) -> Result<Vec<String>> {
    let wanted: Vec<Bg3Mod> = active
        .iter()
        .filter_map(|m| m.special.get(INFO))
        .flat_map(|i| bg3_mods(i))
        .collect();
    let profiles = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir() && !e.file_name().eq_ignore_ascii_case("default"))
        .map(|e| e.path().join("modsettings.lsx"))
        .filter(|p| p.is_file());
    for file in profiles {
        let text = read_text(&file)?;
        let new = bg3_edit(&text, &wanted, before)?;
        if new != text {
            write_durable(&file, new.as_bytes())?;
        }
    }
    Ok(wanted.into_iter().map(|m| m.uuid).collect())
}

/// modsettings.lsx with `wanted` listed under `node id="Mods"` / `children`, and UUIDs in
/// `before` but not wanted removed.
fn bg3_edit(text: &str, wanted: &[Bg3Mod], before: &BTreeSet<String>) -> Result<String> {
    let doc = parse(text)?;
    let off = bom(text);
    let mods = doc
        .descendants()
        .find(|n| n.has_tag_name("node") && n.attribute("id") == Some("Mods"))
        .ok_or("modsettings.lsx has no Mods node")?;
    let children = mods
        .children()
        .find(|n| n.has_tag_name("children"))
        .ok_or("modsettings.lsx Mods node has no children")?;
    let uuid_of = |n: roxmltree::Node| {
        n.children()
            .find(|a| a.has_tag_name("attribute") && a.attribute("id") == Some("UUID"))
            .and_then(|a| a.attribute("value"))
            .map(String::from)
    };
    let present: Vec<(String, std::ops::Range<usize>)> = children
        .children()
        .filter(|n| n.has_tag_name("node") && n.attribute("id") == Some("ModuleShortDesc"))
        .filter_map(|n| Some((uuid_of(n)?, n.range())))
        .collect();
    let keep: BTreeSet<&str> = wanted.iter().map(|m| m.uuid.as_str()).collect();
    let cut: Vec<_> = present
        .iter()
        .filter(|(u, _)| before.contains(u) && !keep.contains(u.as_str()))
        .map(|(_, r)| r.start + off..r.end + off)
        .collect();
    let has = |u: &str| present.iter().any(|(p, _)| p == u);
    let r = children.range();
    let (r, child_text) = (
        r.start + off..r.end + off,
        &text[r.start + off..r.end + off],
    );
    let self_closing = child_text.ends_with("/>");
    let ind = indent_at(text, r.start).to_string();
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut add = String::new();
    for m in wanted.iter().filter(|m| !has(&m.uuid)) {
        let i = format!("{ind}\t");
        let attr = |id: &str, ty: &str, v: &str| {
            format!(
                "{nl}{i}\t<attribute id=\"{id}\" type=\"{ty}\" value=\"{}\" />",
                esc(v)
            )
        };
        add.push_str(&format!(
            "{nl}{i}<node id=\"ModuleShortDesc\">{}{}{}{}{}{nl}{i}</node>",
            attr("Folder", "LSString", &m.folder),
            attr("MD5", "LSString", ""),
            attr("Name", "LSString", &m.name),
            attr("UUID", "FixedString", &m.uuid),
            attr("Version64", "int64", &m.version),
        ));
    }
    if add.is_empty() && cut.is_empty() {
        return Ok(text.into());
    }
    if self_closing {
        // `<children />` -> `<children>...</children>`; nothing to cut inside it.
        return Ok(format!(
            "{}<children>{add}{nl}{ind}</children>{}",
            &text[..r.start],
            &text[r.end..]
        ));
    }
    let close = r.start + child_text.rfind("</").unwrap();
    // Insert after the last child, before the closing tag's own indentation.
    let at = text[..close]
        .trim_end_matches([' ', '\t', '\r', '\n'])
        .len();
    Ok(splice(text, cut, at, &add))
}

// ---------- Dragon Age AddIns.xml ----------

fn addins_sync(
    file: &Path,
    active: &[&ModEntry],
    before: &BTreeSet<String>,
) -> Result<Vec<String>> {
    // (UID, raw AddInItem xml) from every active .dazip's manifest.
    let mut wanted: Vec<(String, String)> = vec![];
    for m in active {
        let Some(manifest) = m.special.get(MANIFEST) else {
            continue;
        };
        let Ok(doc) = parse(manifest) else {
            tracing::warn!(mod_id = %m.id, "unreadable Manifest.xml");
            continue;
        };
        let off = bom(manifest);
        for item in doc.descendants().filter(|n| {
            n.has_tag_name("AddInItem") && n.parent().is_some_and(|p| p.has_tag_name("AddInsList"))
        }) {
            if let Some(uid) = item.attribute("UID") {
                let r = item.range();
                wanted.push((uid.into(), manifest[r.start + off..r.end + off].into()));
            }
        }
    }
    if wanted.is_empty() && before.is_empty() {
        return Ok(vec![]);
    }
    let text = read_text(file)?;
    let doc = parse(&text)?;
    let off = bom(&text);
    let root = doc.root_element();
    let items: Vec<(String, std::ops::Range<usize>)> = root
        .descendants()
        .filter(|n| n.has_tag_name("AddInItem"))
        .filter_map(|n| Some((n.attribute("UID")?.to_string(), n.range())))
        .collect();
    let keep: BTreeSet<&str> = wanted.iter().map(|(u, _)| u.as_str()).collect();
    let cut: Vec<_> = items
        .iter()
        .filter(|(u, _)| {
            before.iter().any(|b| b.eq_ignore_ascii_case(u))
                && !keep.iter().any(|k| k.eq_ignore_ascii_case(u))
        })
        .map(|(_, r)| r.start + off..r.end + off)
        .collect();
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let r = root.range();
    let ind = format!("{}\t", indent_at(&text, r.start + off));
    let add: String = wanted
        .iter()
        .filter(|(u, _)| !items.iter().any(|(i, _)| i.eq_ignore_ascii_case(u)))
        .map(|(_, xml)| format!("{nl}{ind}{}", xml.trim()))
        .collect();
    let root_text = &text[r.start + off..r.end + off];
    let new = if root_text.ends_with("/>") {
        let name = root.tag_name().name();
        let open = text[..r.end + off - 2].trim_end();
        format!("{open}>{add}{nl}</{name}>{}", &text[r.end + off..])
    } else {
        let close = r.start + off + root_text.rfind("</").unwrap();
        let at = text[..close]
            .trim_end_matches([' ', '\t', '\r', '\n'])
            .len();
        splice(&text, cut, at, &add)
    };
    if new != text {
        write_durable(file, new.as_bytes())?;
    }
    Ok(wanted.into_iter().map(|(u, _)| u).collect())
}

// ---------- chargenmorphcfg.xml ----------

#[derive(Clone, Debug, PartialEq)]
enum Node {
    El(El),
    Text(String),
}

#[derive(Clone, Debug, PartialEq)]
struct El {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
}

fn to_el(n: roxmltree::Node) -> El {
    El {
        name: n.tag_name().name().into(),
        attrs: n
            .attributes()
            .map(|a| (a.name().into(), a.value().into()))
            .collect(),
        children: n
            .children()
            .filter_map(|c| {
                if c.is_element() {
                    Some(Node::El(to_el(c)))
                } else if c.is_text() {
                    let t = c.text()?.trim();
                    (!t.is_empty()).then(|| Node::Text(t.into()))
                } else {
                    None
                }
            })
            .collect(),
    }
}

/// Same element for merging (C# `AreEquivalent`): name and the same attribute set.
fn same(a: &El, b: &El) -> bool {
    a.name == b.name
        && a.attrs.len() == b.attrs.len()
        && a.attrs.iter().all(|x| b.attrs.contains(x))
}

/// C# `MergeElements`: `b`'s elements go into `a`, recursing into equivalent ones.
// ponytail: C# walked DescendantNodes and threw (swallowed) on the first text node; this merges the tree.
fn merge(a: &mut El, b: &El) {
    for bc in &b.children {
        let Node::El(be) = bc else { continue };
        match a.children.iter_mut().find_map(|c| match c {
            Node::El(ae) if same(ae, be) => Some(ae),
            _ => None,
        }) {
            Some(ae) => merge(ae, be),
            None => a.children.push(bc.clone()),
        }
    }
}

fn write_el(e: &El, depth: usize, out: &mut String) {
    let ind = "\t".repeat(depth);
    out.push_str(&format!("{ind}<{}", e.name));
    for (k, v) in &e.attrs {
        out.push_str(&format!(" {k}=\"{}\"", esc(v)));
    }
    match e.children.as_slice() {
        [] => out.push_str(" />\r\n"),
        [Node::Text(t)] => out.push_str(&format!(">{}</{}>\r\n", esc(t), e.name)),
        kids => {
            out.push_str(">\r\n");
            for k in kids {
                match k {
                    Node::El(c) => write_el(c, depth + 1, out),
                    Node::Text(t) => out.push_str(&format!("{ind}\t{}\r\n", esc(t))),
                }
            }
            out.push_str(&format!("{ind}</{}>\r\n", e.name));
        }
    }
}

fn morph_sync(out: &Path, sources: &[PathBuf]) -> Result<()> {
    let mut merged: Option<El> = None;
    for src in sources {
        // C# stripped "???" (mis-decoded BOMs) before parsing.
        let text = read_text(src)?.replace("???", "");
        let doc = parse(&text)?;
        let el = to_el(doc.root_element());
        match &mut merged {
            None => merged = Some(el),
            Some(m) => merge(m, &el),
        }
    }
    match merged {
        None => match fs::remove_file(out) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(crate::Error::Io {
                path: out.into(),
                source: e,
            }),
            _ => Ok(()),
        },
        Some(m) => {
            let mut s = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n");
            write_el(&m, 0, &mut s);
            if fs::read(out).is_ok_and(|old| old == s.as_bytes()) {
                return Ok(());
            }
            let dir = out.parent().unwrap();
            fs::create_dir_all(dir).at(dir)?;
            write_durable(out, s.as_bytes())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, special: &[(&str, &str)], files: &[&str]) -> ModEntry {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "archive": "", "active": true,
            "files": files,
            "special": special.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
        }))
        .unwrap()
    }

    const LSX: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n<save>\r\n\t<region id=\"ModuleSettings\">\r\n\t\t<node id=\"root\">\r\n\t\t\t<children>\r\n\t\t\t\t<node id=\"Mods\">\r\n\t\t\t\t\t<children>\r\n\t\t\t\t\t\t<node id=\"ModuleShortDesc\">\r\n\t\t\t\t\t\t\t<attribute id=\"Folder\" type=\"LSString\" value=\"GustavDev\" />\r\n\t\t\t\t\t\t\t<attribute id=\"UUID\" type=\"FixedString\" value=\"gustav\" />\r\n\t\t\t\t\t\t</node>\r\n\t\t\t\t\t</children>\r\n\t\t\t\t</node>\r\n\t\t\t</children>\r\n\t\t</node>\r\n\t</region>\r\n</save>\r\n";

    fn uuids(text: &str) -> Vec<String> {
        let doc = roxmltree::Document::parse(text).unwrap();
        doc.descendants()
            .filter(|n| n.attribute("id") == Some("UUID"))
            .map(|n| n.attribute("value").unwrap().to_string())
            .collect()
    }

    #[test]
    fn bg3_modsettings_follow_active_mods() {
        let t = tempfile::tempdir().unwrap();
        let mut game = crate::game::find("BaldursGate3").unwrap();
        game.user_dir = Some(t.path().into());
        let profiles = t
            .path()
            .join("AppData/Local/Larian Studios/Baldur's Gate 3/PlayerProfiles");
        for p in ["Public", "default"] {
            fs::create_dir_all(profiles.join(p)).unwrap();
            fs::write(profiles.join(p).join("modsettings.lsx"), LSX).unwrap();
        }
        let info = r#"{"Mods":[{"Folder":"Cool","Name":"Cool & Co","UUID":"u-1","Version":"36028797018963968"}]}"#;
        let mut mods = vec![entry("cool", &[(INFO, info)], &[])];
        let mut applied = BTreeMap::new();
        let store = |_: &str, _: &str| PathBuf::new();
        sync(&game, t.path(), &mods, &store, &mut applied);
        let lsx = fs::read_to_string(profiles.join("Public/modsettings.lsx")).unwrap();
        assert_eq!(uuids(&lsx), ["gustav", "u-1"]);
        assert!(
            lsx.contains("value=\"Cool &amp; Co\"")
                && lsx.contains("\r\n\t\t\t\t\t\t<node id=\"ModuleShortDesc\">"),
            "{lsx}"
        );
        assert_eq!(applied[BG3_KEY], ["u-1"]);
        // "default" is left alone; a second sync changes nothing.
        assert_eq!(
            fs::read_to_string(profiles.join("default/modsettings.lsx")).unwrap(),
            LSX
        );
        sync(&game, t.path(), &mods, &store, &mut applied);
        assert_eq!(
            fs::read_to_string(profiles.join("Public/modsettings.lsx")).unwrap(),
            lsx
        );
        // Disabling removes only what we added.
        mods[0].active = false;
        sync(&game, t.path(), &mods, &store, &mut applied);
        assert_eq!(
            fs::read_to_string(profiles.join("Public/modsettings.lsx")).unwrap(),
            LSX
        );
        assert!(applied.is_empty());
    }

    #[test]
    fn dragon_age_addins_follow_active_dazips() {
        let t = tempfile::tempdir().unwrap();
        let mut game = crate::game::find("DragonAge").unwrap();
        game.user_dir = Some(t.path().into());
        let settings = t.path().join("Documents/BioWare/Dragon Age/Settings");
        fs::create_dir_all(&settings).unwrap();
        let base = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<AddInsList>\n\t<AddInItem UID=\"DAO_PRC_CP_1\" Name=\"Stone Prisoner\" />\n</AddInsList>\n";
        fs::write(settings.join("AddIns.xml"), base).unwrap();
        let manifest = "<Manifest><AddInsList><AddInItem UID=\"cool_mod\" Name=\"Cool\"><Title>x</Title></AddInItem></AddInsList></Manifest>";
        let mut mods = vec![entry("cool", &[(MANIFEST, manifest)], &[])];
        let mut applied = BTreeMap::new();
        let store = |_: &str, _: &str| PathBuf::new();
        sync(&game, t.path(), &mods, &store, &mut applied);
        let x = fs::read_to_string(settings.join("AddIns.xml")).unwrap();
        assert!(x.contains("\t<AddInItem UID=\"cool_mod\" Name=\"Cool\"><Title>x</Title></AddInItem>\n</AddInsList>"), "{x}");
        mods.clear();
        sync(&game, t.path(), &mods, &store, &mut applied);
        assert_eq!(
            fs::read_to_string(settings.join("AddIns.xml")).unwrap(),
            base
        );
    }

    #[test]
    fn chargenmorph_merges_active_mods() {
        let t = tempfile::tempdir().unwrap();
        let mut game = crate::game::find("DragonAge2").unwrap();
        game.user_dir = Some(t.path().into());
        let store_dir = t.path().join("store");
        for (id, xml) in [
            ("a", "<chargen><hair><resource name=\"h1\" /></hair></chargen>"),
            ("b", "\u{feff}<chargen><hair><resource name=\"h2\" /><resource name=\"h1\" /></hair><beard><resource name=\"b1\" /></beard></chargen>"),
        ] {
            fs::create_dir_all(store_dir.join(id)).unwrap();
            fs::write(store_dir.join(id).join(MORPH), xml).unwrap();
        }
        let store = |id: &str, f: &str| store_dir.join(id).join(f);
        let mut mods = vec![entry("a", &[], &[MORPH]), entry("b", &[], &[MORPH])];
        let mut applied = BTreeMap::new();
        sync(&game, t.path(), &mods, &store, &mut applied);
        let out = game
            .install_root(t.path())
            .join("NMM_chargenmorphcfg")
            .join(MORPH);
        let merged = fs::read_to_string(&out).unwrap();
        let doc = roxmltree::Document::parse(&merged).unwrap();
        let names: Vec<_> = doc
            .descendants()
            .filter_map(|n| n.attribute("name"))
            .collect();
        assert_eq!(names, ["h1", "h2", "b1"]);
        for m in &mut mods {
            m.active = false;
        }
        sync(&game, t.path(), &mods, &store, &mut applied);
        assert!(!out.exists());
    }

    #[test]
    fn content_xml_save_warning() {
        let saved = entry(
            "m",
            &[(CONTENT, r#"<content name="Big Mod" save="1"/>"#)],
            &[],
        );
        assert!(removal_warning(&saved).unwrap().contains("Big Mod mod"));
        let unsaved = entry(
            "m",
            &[(CONTENT, r#"<content name="x" save="false"/>"#)],
            &[],
        );
        assert_eq!(removal_warning(&unsaved), None);
        assert_eq!(removal_warning(&entry("m", &[], &[])), None);
    }
}
