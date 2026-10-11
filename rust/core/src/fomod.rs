//! FOMod XML installer (`fomod/ModuleConfig.xml`), port of `Script Types/XmlScript`.
//! C# scripts (`fomod/script.cs`) are intentionally unsupported: `locate_script` flags them for a plain install.

use roxmltree::Node;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct ModuleConfig {
    pub name: String,
    /// Header image (`moduleImage`), else the archive's `fomod/screenshot*` (set by `Manager::stage`).
    pub image: Option<String>,
    pub steps: Vec<Step>,
    /// `moduleDependencies`: must hold before the wizard runs.
    #[serde(skip)]
    pub prerequisites: Option<Cond>,
    #[serde(skip)]
    pub required: Vec<FileItem>,
    #[serde(skip)]
    pub conditional: Vec<(Cond, Vec<FileItem>)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub name: String,
    pub groups: Vec<Group>,
    #[serde(skip)]
    pub visible: Option<Cond>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Group {
    pub name: String,
    /// SelectAtLeastOne | SelectAtMostOne | SelectExactlyOne | SelectAll | SelectAny
    pub kind: String,
    pub plugins: Vec<Plugin>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plugin {
    pub name: String,
    pub description: String,
    pub image: Option<String>,
    #[serde(skip)]
    pub files: Vec<FileItem>,
    #[serde(skip)]
    pub flags: Vec<(String, String)>,
    #[serde(skip)]
    pub type_desc: TypeDesc,
}

#[derive(Debug, Clone)]
pub enum TypeDesc {
    Static(String),
    Dependent {
        default: String,
        patterns: Vec<(Cond, String)>,
    },
}

#[derive(Debug, Clone)]
pub struct FileItem {
    pub source: String,
    pub destination: String,
    pub is_folder: bool,
    pub priority: i32,
}

#[derive(Debug, Clone)]
pub enum Cond {
    All(Vec<Cond>),
    Any(Vec<Cond>),
    Flag(String, String),
    File(String, String),
    /// Minimum version of the game, script extender or mod manager.
    Version(Who, Version),
    True,
}

#[derive(Debug, Clone)]
pub enum Who {
    Game,
    /// `skseDependency` etc.: the name, upper-cased (`SKSE`).
    Extender(String),
    Manager,
}

/// Four-part file version (C# `System.Version`, missing parts 0).
pub type Version = [u32; 4];

/// The mod manager version `fommDependency`/`nmmDependency` are checked against: the last C# NMM
/// release (`CommonData.VersionString`), which FOMods were written for.
pub const NMM_VERSION: Version = [0, 92, 14, 0];

/// What conditions are checked against.
pub struct Env {
    /// fileDependency state: Active | Inactive | Missing.
    pub file_state: Box<dyn Fn(&str) -> String>,
    /// File versions of the game / script extender executables; None when not found.
    pub game: Option<Version>,
    pub extender: Option<Version>,
}

/// One remembered wizard choice, by name so it survives option reordering and new versions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub step: String,
    pub group: String,
    pub plugin: String,
}

/// `fomod/info.xml` metadata (C# `FOMod.LoadInfo`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Info {
    pub name: String,
    pub author: String,
    pub version: String,
    /// Nexus mod id.
    pub id: String,
}

/// (step, group, plugin) indices of checked options.
pub type Selection = Vec<(usize, usize, usize)>;

#[derive(Debug, Serialize)]
pub struct Eval {
    pub visible: Vec<bool>,
    /// Plugin type per step/group/plugin: Required, Optional, Recommended, NotUsable, CouldBeUsable.
    pub types: Vec<Vec<Vec<String>>>,
}

// ---------- parsing ----------

fn decode(bytes: &[u8]) -> String {
    let utf16 = |le: bool, b: &[u8]| {
        let units: Vec<u16> = b
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| {
                if le {
                    u16::from_le_bytes(c)
                } else {
                    u16::from_be_bytes(c)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(true, rest),
        [0xFE, 0xFF, rest @ ..] => utf16(false, rest),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [b'<', 0, ..] => utf16(true, bytes),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name(name))
}

fn children<'a, 'i>(n: Node<'a, 'i>, name: &'a str) -> impl Iterator<Item = Node<'a, 'i>> + 'a {
    n.children().filter(move |c| c.has_tag_name(name))
}

fn text(n: Option<Node>) -> String {
    n.and_then(|n| n.text()).unwrap_or("").trim().to_string()
}

fn sort_by_order<T>(items: &mut [T], order: Option<&str>, name: impl Fn(&T) -> String) {
    match order.unwrap_or("Ascending") {
        "Explicit" => {}
        "Descending" => items.sort_by_key(|i| std::cmp::Reverse(name(i).to_lowercase())),
        _ => items.sort_by_key(|i| name(i).to_lowercase()),
    }
}

fn parse_cond(n: Node) -> Cond {
    let parts: Vec<Cond> = n
        .children()
        .filter(|c| c.is_element())
        .map(|c| match c.tag_name().name() {
            "flagDependency" => Cond::Flag(
                c.attribute("flag").unwrap_or("").into(),
                c.attribute("value").unwrap_or("").into(),
            ),
            "fileDependency" => Cond::File(
                c.attribute("file").unwrap_or("").into(),
                c.attribute("state").unwrap_or("Active").into(),
            ),
            // Pre-3.0 configs.
            "moduleFileDependency" => {
                Cond::File(c.attribute("file").unwrap_or("").into(), "Active".into())
            }
            "dependencies" | "dependancies" => parse_cond(c),
            tag => {
                let v = parse_version(c.attribute("version").unwrap_or(""));
                match tag {
                    "gameDependency" | "falloutDependency" => Cond::Version(Who::Game, v),
                    "fommDependency" | "nmmDependency" => Cond::Version(Who::Manager, v),
                    // foseDependency, nvseDependency, skseDependency, ...
                    _ if tag.ends_with("seDependency") => Cond::Version(
                        Who::Extender(tag.trim_end_matches("Dependency").to_uppercase()),
                        v,
                    ),
                    _ => Cond::True,
                }
            }
        })
        .collect();
    if n.attribute("operator") == Some("Or") {
        Cond::Any(parts)
    } else {
        Cond::All(parts)
    }
}

fn parse_files(n: Option<Node>) -> Vec<FileItem> {
    let Some(n) = n else { return vec![] };
    n.children()
        .filter(|c| c.has_tag_name("file") || c.has_tag_name("folder"))
        .map(|c| {
            let source = c.attribute("source").unwrap_or("").replace('\\', "/");
            FileItem {
                destination: c
                    .attribute("destination")
                    .map(|d| d.replace('\\', "/"))
                    .unwrap_or_else(|| source.clone()),
                source,
                is_folder: c.has_tag_name("folder"),
                priority: c
                    .attribute("priority")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(0),
            }
        })
        .collect()
}

fn type_name(n: Option<Node>) -> String {
    n.and_then(|n| n.attribute("name"))
        .unwrap_or("Optional")
        .to_string()
}

fn parse_plugin(p: Node) -> Plugin {
    let td = child(p, "typeDescriptor");
    let type_desc = match td.and_then(|t| child(t, "dependencyType")) {
        Some(dt) => TypeDesc::Dependent {
            default: type_name(child(dt, "defaultType")),
            patterns: child(dt, "patterns")
                .map(|ps| {
                    children(ps, "pattern")
                        .map(|pt| {
                            let deps =
                                child(pt, "dependencies").or_else(|| child(pt, "dependancies"));
                            (
                                deps.map(parse_cond).unwrap_or(Cond::True),
                                type_name(child(pt, "type")),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
        },
        None => TypeDesc::Static(type_name(td.and_then(|t| child(t, "type")))),
    };
    Plugin {
        name: p.attribute("name").unwrap_or("").into(),
        description: text(child(p, "description")),
        image: child(p, "image")
            .and_then(|i| i.attribute("path"))
            .map(|s| s.replace('\\', "/")),
        files: parse_files(child(p, "files")),
        flags: child(p, "conditionFlags")
            .map(|f| {
                children(f, "flag")
                    .map(|fl| {
                        (
                            fl.attribute("name").unwrap_or("").into(),
                            fl.text().unwrap_or("").into(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        type_desc,
    }
}

fn parse_groups(n: Node) -> Vec<Group> {
    let mut groups: Vec<Group> = children(n, "group")
        .map(|g| {
            let plugins_el = child(g, "plugins");
            let mut plugins: Vec<Plugin> = plugins_el
                .map(|ps| children(ps, "plugin").map(parse_plugin).collect())
                .unwrap_or_default();
            sort_by_order(
                &mut plugins,
                plugins_el.and_then(|p| p.attribute("order")),
                |p| p.name.clone(),
            );
            Group {
                name: g.attribute("name").unwrap_or("").into(),
                kind: g.attribute("type").unwrap_or("SelectAny").into(),
                plugins,
            }
        })
        .collect();
    sort_by_order(&mut groups, n.attribute("order"), |g| g.name.clone());
    groups
}

/// C# `Parser.ParseVersion`: 2-4 numeric parts, anything else 0.0.0.0.
pub fn parse_version(s: &str) -> Version {
    let parts: Option<Vec<u32>> = s.trim().split('.').map(|p| p.parse().ok()).collect();
    match parts {
        Some(p) if (2..=4).contains(&p.len()) => {
            let mut v = [0; 4];
            v[..p.len()].copy_from_slice(&p);
            v
        }
        _ => [0; 4],
    }
}

fn show(v: &Version) -> String {
    v.map(|n| n.to_string()).join(".")
}

fn parse_doc(xml: &str) -> Result<roxmltree::Document<'_>, roxmltree::Error> {
    roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
}

/// `fomod/info.xml`; None when unreadable or empty.
pub fn parse_info(xml_bytes: &[u8]) -> Option<Info> {
    let xml = decode(xml_bytes);
    let doc = parse_doc(&xml)
        .inspect_err(|e| tracing::warn!("fomod/info.xml: {e}"))
        .ok()?;
    let root = doc.root_element();
    let get = |n: &str| text(child(root, n));
    let info = Info {
        name: get("Name"),
        author: get("Author"),
        version: get("Version"),
        id: get("Id"),
    };
    (info != Info::default()).then_some(info)
}

pub fn parse(xml_bytes: &[u8]) -> crate::Result<ModuleConfig> {
    let xml = decode(xml_bytes);
    let doc = parse_doc(&xml).map_err(|e| format!("fomod/ModuleConfig.xml: {e}"))?;
    let root = doc.root_element();

    let mut steps: Vec<Step> = match child(root, "installSteps") {
        Some(is) => {
            let mut s: Vec<Step> = children(is, "installStep")
                .map(|st| Step {
                    name: st.attribute("name").unwrap_or("").into(),
                    visible: child(st, "visible").map(parse_cond),
                    groups: child(st, "optionalFileGroups")
                        .map(parse_groups)
                        .unwrap_or_default(),
                })
                .collect();
            sort_by_order(&mut s, is.attribute("order"), |s| s.name.clone());
            s
        }
        // Pre-3.0 configs: groups hang off the root.
        None => child(root, "optionalFileGroups")
            .map(|g| {
                vec![Step {
                    name: "Options".into(),
                    visible: None,
                    groups: parse_groups(g),
                }]
            })
            .unwrap_or_default(),
    };
    steps.retain(|s| !s.groups.is_empty());

    let conditional = child(root, "conditionalFileInstalls")
        .and_then(|c| child(c, "patterns"))
        .map(|ps| {
            children(ps, "pattern")
                .map(|p| {
                    let deps = child(p, "dependencies").or_else(|| child(p, "dependancies"));
                    (
                        deps.map(parse_cond).unwrap_or(Cond::True),
                        parse_files(child(p, "files")),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(ModuleConfig {
        name: text(child(root, "moduleName")),
        image: child(root, "moduleImage")
            .filter(|i| i.attribute("showImage") != Some("false"))
            .and_then(|i| i.attribute("path"))
            .filter(|p| !p.is_empty())
            .map(|p| p.replace('\\', "/")),
        prerequisites: child(root, "moduleDependencies").map(parse_cond),
        required: parse_files(child(root, "requiredInstallFiles")),
        steps,
        conditional,
    })
}

// ---------- evaluation ----------

type Flags = HashMap<String, String>;

fn installed(w: &Who, env: &Env) -> Option<Version> {
    match w {
        Who::Game => env.game,
        Who::Extender(_) => env.extender,
        Who::Manager => Some(NMM_VERSION),
    }
}

fn holds(c: &Cond, flags: &Flags, env: &Env) -> bool {
    match c {
        Cond::All(v) => v.iter().all(|c| holds(c, flags, env)),
        Cond::Any(v) => v.iter().any(|c| holds(c, flags, env)),
        Cond::Flag(f, v) => flags.get(f).map(String::as_str).unwrap_or("") == v,
        Cond::File(f, s) => (env.file_state)(f).eq_ignore_ascii_case(s),
        Cond::Version(w, min) => installed(w, env).is_some_and(|v| v >= *min),
        Cond::True => true,
    }
}

/// Why `c` fails (C# `GetMessage`); empty when it holds.
fn unmet(c: &Cond, flags: &Flags, env: &Env) -> Vec<String> {
    if holds(c, flags, env) {
        return vec![];
    }
    match c {
        Cond::All(v) | Cond::Any(v) => v.iter().flat_map(|c| unmet(c, flags, env)).collect(),
        Cond::Flag(f, v) => vec![format!("Flag '{f}' is not '{v}'.")],
        Cond::File(f, s) => vec![format!("File '{f}' is not {s}.")],
        Cond::Version(w, min) => {
            let what = match w {
                Who::Game => "the game".to_string(),
                Who::Extender(n) => n.clone(),
                Who::Manager => "the mod manager".to_string(),
            };
            let have =
                installed(w, env).map_or("none found".into(), |v| format!("you have {}", show(&v)));
            vec![format!(
                "This mod requires {what} v{} or higher ({have}).",
                show(min)
            )]
        }
        Cond::True => vec![],
    }
}

impl ModuleConfig {
    /// Walk steps in order; returns flags *before* each step, visibility, and final flags.
    fn walk(&self, sel: &Selection, fs: &Env) -> (Vec<Flags>, Vec<bool>, Flags) {
        let mut flags = Flags::new();
        let (mut before, mut visible) = (vec![], vec![]);
        for (si, step) in self.steps.iter().enumerate() {
            before.push(flags.clone());
            let vis = step.visible.as_ref().is_none_or(|c| holds(c, &flags, fs));
            visible.push(vis);
            if vis {
                for &(_, gi, pi) in sel.iter().filter(|s| s.0 == si) {
                    if let Some(p) = self.steps[si]
                        .groups
                        .get(gi)
                        .and_then(|g| g.plugins.get(pi))
                    {
                        flags.extend(p.flags.iter().cloned());
                    }
                }
            }
        }
        (before, visible, flags)
    }

    /// Unmet `moduleDependencies`, as a message; C# refuses to run the installer.
    pub fn unmet(&self, env: &Env) -> Option<String> {
        let why = unmet(self.prerequisites.as_ref()?, &Flags::new(), env);
        (!why.is_empty()).then(|| why.join(" "))
    }

    pub fn evaluate(&self, sel: &Selection, fs: &Env) -> Eval {
        let (before, visible, _) = self.walk(sel, fs);
        let types = self
            .steps
            .iter()
            .zip(&before)
            .map(|(step, flags)| {
                step.groups
                    .iter()
                    .map(|g| {
                        g.plugins
                            .iter()
                            .map(|p| match &p.type_desc {
                                TypeDesc::Static(t) => t.clone(),
                                TypeDesc::Dependent { default, patterns } => patterns
                                    .iter()
                                    .find(|(c, _)| holds(c, flags, fs))
                                    .map(|(_, t)| t.clone())
                                    .unwrap_or_else(|| default.clone()),
                            })
                            .collect()
                    })
                    .collect()
            })
            .collect();
        Eval { visible, types }
    }

    /// Files to install for a selection, lowest priority first (later entries win).
    pub fn files(&self, sel: &Selection, fs: &Env) -> Vec<FileItem> {
        let (_, visible, flags) = self.walk(sel, fs);
        let mut out = self.required.clone();
        for &(si, gi, pi) in sel {
            if visible.get(si) == Some(&true) {
                if let Some(p) = self.steps[si]
                    .groups
                    .get(gi)
                    .and_then(|g| g.plugins.get(pi))
                {
                    out.extend(p.files.iter().cloned());
                }
            }
        }
        for (c, files) in &self.conditional {
            if holds(c, &flags, fs) {
                out.extend(files.iter().cloned());
            }
        }
        out.sort_by_key(|f| f.priority); // stable: document order breaks ties
        out
    }

    /// `sel` by name, to remember.
    pub fn choices(&self, sel: &Selection) -> Vec<Choice> {
        sel.iter()
            .filter_map(|&(si, gi, pi)| {
                let s = self.steps.get(si)?;
                let g = s.groups.get(gi)?;
                Some(Choice {
                    step: s.name.clone(),
                    group: g.name.clone(),
                    plugin: g.plugins.get(pi)?.name.clone(),
                })
            })
            .collect()
    }

    /// Remembered `choices` mapped onto this config; ones it no longer has are dropped.
    pub fn selection(&self, choices: &[Choice]) -> Selection {
        choices
            .iter()
            .filter_map(|c| {
                let si = self.steps.iter().position(|s| s.name == c.step)?;
                let gi = self.steps[si]
                    .groups
                    .iter()
                    .position(|g| g.name == c.group)?;
                let pi = self.steps[si].groups[gi]
                    .plugins
                    .iter()
                    .position(|p| p.name == c.plugin)?;
                Some((si, gi, pi))
            })
            .collect()
    }
}

/// Windows executable's file version (`VS_FIXEDFILEINFO`, C# `FileVersionInfo`); reads only the
/// PE headers and resource section. None when missing or not a PE file.
pub fn exe_version(path: &Path) -> Option<Version> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut at = |off: u64, len: usize| -> Option<Vec<u8>> {
        let mut b = vec![0; len];
        f.seek(SeekFrom::Start(off)).ok()?;
        f.read_exact(&mut b).ok()?;
        Some(b)
    };
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    let dos = at(0, 64)?;
    let pe = u32_at(&dos, 0x3c) as u64;
    let coff = at(pe, 24)?;
    if &coff[..4] != b"PE\0\0" {
        return None;
    }
    let (sections, opt_len) = (u16_at(&coff, 6) as usize, u16_at(&coff, 20) as usize);
    let opt = at(pe + 24, opt_len)?;
    // Data directory 2 = resources; the directories start at 96 (PE32) or 112 (PE32+).
    let dirs = if u16_at(&opt, 0) == 0x20b { 112 } else { 96 } + 2 * 8;
    if opt.len() < dirs + 8 {
        return None;
    }
    let (rva, size) = (u32_at(&opt, dirs), u32_at(&opt, dirs + 4));
    let table = at(pe + 24 + opt_len as u64, sections * 40)?;
    let off = table.chunks(40).find_map(|s| {
        let (va, len, raw) = (
            u32_at(s, 12),
            u32_at(s, 8).max(u32_at(s, 16)),
            u32_at(s, 20),
        );
        (va <= rva && rva < va + len).then(|| (raw + rva - va) as u64)
    })?;
    let rsrc = at(off, size.min(64 << 20) as usize)?;
    let i = rsrc
        .windows(4)
        .position(|w| w == 0xFEEF04BDu32.to_le_bytes())?;
    let (ms, ls) = (u32_at(&rsrc, i + 8), u32_at(&rsrc, i + 12));
    Some([ms >> 16, ms & 0xffff, ls >> 16, ls & 0xffff])
}

/// Shallowest `<dir>/fomod/<name>` (case-insensitive) under `root`.
fn find(root: &Path, name: &str) -> Option<PathBuf> {
    walkdir::WalkDir::new(root)
        .sort_by_key(|e| e.depth())
        .into_iter()
        .filter_map(Result::ok)
        .find(|e| {
            e.file_name().eq_ignore_ascii_case(name)
                && e.path()
                    .parent()
                    .and_then(|p| p.file_name())
                    .is_some_and(|n| n.eq_ignore_ascii_case("fomod"))
        })
        .map(|e| e.into_path())
}

/// Shallowest `<dir>/fomod/ModuleConfig.xml` (case-insensitive) under `root`; returns (base dir, config path).
pub fn locate(root: &Path) -> Option<(PathBuf, PathBuf)> {
    find(root, "moduleconfig.xml").map(|p| (p.parent().unwrap().parent().unwrap().to_path_buf(), p))
}

/// Shallowest `fomod/script.cs` (C# script installer, unsupported) under `root`.
pub fn locate_script(root: &Path) -> Option<PathBuf> {
    find(root, "script.cs")
}

/// Shallowest `fomod/info.xml` under `root`.
pub fn locate_info(root: &Path) -> Option<PathBuf> {
    find(root, "info.xml")
}

/// Shallowest `fomod/screenshot*` under `root` (C# `DetectScreenshot`).
pub fn locate_screenshot(root: &Path) -> Option<PathBuf> {
    walkdir::WalkDir::new(root)
        .sort_by(|a, b| {
            a.depth()
                .cmp(&b.depth())
                .then(a.file_name().cmp(b.file_name()))
        })
        .into_iter()
        .filter_map(Result::ok)
        .find(|e| {
            e.file_type().is_file()
                && e.file_name()
                    .to_string_lossy()
                    .to_lowercase()
                    .starts_with("screenshot")
                && e.path()
                    .parent()
                    .and_then(|p| p.file_name())
                    .is_some_and(|n| n.eq_ignore_ascii_case("fomod"))
        })
        .map(|e| e.into_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0" encoding="UTF-16"?>
<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <moduleName>Test</moduleName>
  <requiredInstallFiles><folder source="core" destination="" /></requiredInstallFiles>
  <installSteps order="Explicit">
    <installStep name="Body">
      <optionalFileGroups>
        <group name="Body" type="SelectExactlyOne">
          <plugins order="Explicit">
            <plugin name="CBBE"><description>curvy</description>
              <files><file source="cbbe\body.nif" destination="meshes\body.nif" priority="1"/></files>
              <conditionFlags><flag name="body">cbbe</flag></conditionFlags>
              <typeDescriptor><type name="Recommended"/></typeDescriptor></plugin>
            <plugin name="UNP"><description/>
              <conditionFlags><flag name="body">unp</flag></conditionFlags>
              <typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
          </plugins>
        </group>
      </optionalFileGroups>
    </installStep>
    <installStep name="CBBE extras">
      <visible><flagDependency flag="body" value="cbbe"/></visible>
      <optionalFileGroups><group name="x" type="SelectAny"><plugins><plugin name="extra">
        <description/><typeDescriptor><dependencyType><defaultType name="Optional"/>
        <patterns><pattern><dependencies><fileDependency file="Dawnguard.esm" state="Active"/></dependencies><type name="Required"/></pattern></patterns>
        </dependencyType></typeDescriptor></plugin></plugins></group></optionalFileGroups>
    </installStep>
  </installSteps>
  <conditionalFileInstalls><patterns><pattern>
    <dependencies operator="And"><flagDependency flag="body" value="unp"/></dependencies>
    <files><file source="unp.esp"/></files>
  </pattern></patterns></conditionalFileInstalls>
</config>"#;

    #[test]
    fn flags_drive_visibility_types_and_files() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(XML.encode_utf16().flat_map(|u| u.to_le_bytes()));
        let cfg = parse(&bytes).unwrap();
        assert_eq!(cfg.name, "Test");
        let fs = env(None, None);

        let cbbe: Selection = vec![(0, 0, 0)];
        let e = cfg.evaluate(&cbbe, &fs);
        assert_eq!(e.visible, vec![true, true]);
        assert_eq!(e.types[1][0][0], "Required");
        let files = cfg.files(&cbbe, &fs);
        assert_eq!(
            files
                .iter()
                .map(|f| f.destination.as_str())
                .collect::<Vec<_>>(),
            ["", "meshes/body.nif"]
        );

        let unp: Selection = vec![(0, 0, 1), (1, 0, 0)];
        assert_eq!(cfg.evaluate(&unp, &fs).visible, vec![true, false]);
        let files = cfg.files(&unp, &fs);
        assert_eq!(files.last().unwrap().source, "unp.esp");
        assert_eq!(files.last().unwrap().destination, "unp.esp");
    }

    fn env(game: Option<Version>, extender: Option<Version>) -> Env {
        Env {
            file_state: Box::new(|f: &str| {
                match f {
                    "Dawnguard.esm" => "Active",
                    "Off.esp" => "Inactive",
                    _ => "Missing",
                }
                .into()
            }),
            game,
            extender,
        }
    }

    #[test]
    fn version_and_file_state_conditions() {
        let cfg = parse(
            br#"<config><moduleName>V</moduleName>
  <moduleImage path="fomod\header.png"/>
  <moduleDependencies operator="And">
    <gameDependency version="1.5.97"/>
    <skseDependency version="2.0.17"/>
    <fommDependency version="0.13.21"/>
  </moduleDependencies>
  <installSteps><installStep name="S"><optionalFileGroups><group name="G" type="SelectAny"><plugins order="Explicit">
    <plugin name="new game"><description/><typeDescriptor><dependencyType><defaultType name="NotUsable"/><patterns>
      <pattern><dependencies><gameDependency version="1.6"/></dependencies><type name="Recommended"/></pattern></patterns>
    </dependencyType></typeDescriptor></plugin>
    <plugin name="patch"><description/><typeDescriptor><dependencyType><defaultType name="Optional"/><patterns>
      <pattern><dependencies><fileDependency file="Off.esp" state="Inactive"/></dependencies><type name="Required"/></pattern></patterns>
    </dependencyType></typeDescriptor></plugin>
    <plugin name="nmm"><description/><typeDescriptor><dependencyType><defaultType name="NotUsable"/><patterns>
      <pattern><dependencies><nmmDependency version="0.92"/></dependencies><type name="Optional"/></pattern></patterns>
    </dependencyType></typeDescriptor></plugin>
  </plugins></group></optionalFileGroups></installStep></installSteps></config>"#,
        )
        .unwrap();
        assert_eq!(cfg.image.as_deref(), Some("fomod/header.png"));
        assert_eq!(parse_version("1.5.97"), [1, 5, 97, 0]);
        assert_eq!(parse_version("garbage"), [0; 4]);

        let ok = env(Some([1, 6, 640, 0]), Some([2, 2, 3, 0]));
        assert_eq!(cfg.unmet(&ok), None);
        assert_eq!(
            cfg.evaluate(&vec![], &ok).types[0][0],
            ["Recommended", "Required", "Optional"]
        );

        let old = env(Some([1, 5, 80, 0]), None);
        assert_eq!(
            cfg.unmet(&old).unwrap(),
            "This mod requires the game v1.5.97.0 or higher (you have 1.5.80.0). \
             This mod requires SKSE v2.0.17.0 or higher (none found)."
        );
        assert_eq!(cfg.evaluate(&vec![], &old).types[0][0][0], "NotUsable");
    }

    #[test]
    fn info_xml_and_screenshot() {
        let info = parse_info(
            "<?xml version=\"1.0\"?><fomod><Name>Cool Mod</Name><Author>Me</Author>\
             <Version MachineVersion=\"1.2\">1.2b</Version><Id>1234</Id><Groups/></fomod>"
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            info,
            Info {
                name: "Cool Mod".into(),
                author: "Me".into(),
                version: "1.2b".into(),
                id: "1234".into(),
            }
        );
        assert_eq!(parse_info(b"<fomod/>"), None);
        assert_eq!(parse_info(b"not xml"), None);

        let t = tempfile::tempdir().unwrap();
        for f in [
            "Mod/fomod/info.xml",
            "Mod/fomod/Screenshot.PNG",
            "Mod/screenshot.jpg",
        ] {
            let p = t.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        let shot = locate_screenshot(t.path()).unwrap();
        assert!(shot.ends_with("Mod/fomod/Screenshot.PNG"));
        assert!(locate_info(t.path())
            .unwrap()
            .ends_with("Mod/fomod/info.xml"));
    }

    #[test]
    fn exe_version_from_pe_resources() {
        // DOS header -> PE32 header with one section holding the resources at file offset 512.
        let mut b = vec![0u8; 1024];
        b[0x3c..0x40].copy_from_slice(&64u32.to_le_bytes());
        b[64..68].copy_from_slice(b"PE\0\0");
        b[64 + 6..64 + 8].copy_from_slice(&1u16.to_le_bytes()); // sections
        b[64 + 20..64 + 22].copy_from_slice(&224u16.to_le_bytes()); // optional header size
        let opt = 64 + 24;
        b[opt..opt + 2].copy_from_slice(&0x10bu16.to_le_bytes());
        b[opt + 112..opt + 116].copy_from_slice(&0x2000u32.to_le_bytes()); // resource rva
        b[opt + 116..opt + 120].copy_from_slice(&256u32.to_le_bytes());
        let sec = opt + 224;
        for (i, v) in [(8, 256u32), (12, 0x2000), (16, 256), (20, 512)] {
            b[sec + i..sec + i + 4].copy_from_slice(&v.to_le_bytes());
        }
        let fixed = 512 + 40;
        b[fixed..fixed + 4].copy_from_slice(&0xFEEF04BDu32.to_le_bytes());
        b[fixed + 8..fixed + 12].copy_from_slice(&((1u32 << 16) | 6).to_le_bytes());
        b[fixed + 12..fixed + 16].copy_from_slice(&((1170u32 << 16) | 2).to_le_bytes());
        let t = tempfile::tempdir().unwrap();
        let exe = t.path().join("Game.exe");
        std::fs::write(&exe, &b).unwrap();
        assert_eq!(exe_version(&exe), Some([1, 6, 1170, 2]));
        std::fs::write(&exe, b"#!/bin/sh").unwrap();
        assert_eq!(exe_version(&exe), None);
        assert_eq!(exe_version(&t.path().join("missing.exe")), None);
    }

    #[test]
    fn choices_survive_reordered_config() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(XML.encode_utf16().flat_map(|u| u.to_le_bytes()));
        let cfg = parse(&bytes).unwrap();
        let choices = cfg.choices(&vec![(0, 0, 1), (1, 0, 0), (9, 9, 9)]);
        assert_eq!(
            choices[0],
            Choice {
                step: "Body".into(),
                group: "Body".into(),
                plugin: "UNP".into()
            }
        );
        assert_eq!(choices.len(), 2);
        // A newer version lists UNP first and drops the extras step.
        let newer = XML
            .replace(
                r#"<plugins order="Explicit">"#,
                r#"<plugins order="Descending">"#,
            )
            .replace("CBBE extras", "Gone");
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(newer.encode_utf16().flat_map(|u| u.to_le_bytes()));
        let cfg2 = parse(&bytes).unwrap();
        assert_eq!(cfg2.steps[0].groups[0].plugins[0].name, "UNP");
        assert_eq!(cfg2.selection(&choices), vec![(0, 0, 0)]);
    }
}
