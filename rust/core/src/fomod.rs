//! FOMod XML installer (`fomod/ModuleConfig.xml`), port of `Script Types/XmlScript`.
//! C# scripts (`fomod/script.cs`) are intentionally unsupported.

use roxmltree::Node;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct ModuleConfig {
    pub name: String,
    pub steps: Vec<Step>,
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
    True,
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
            "dependencies" | "dependancies" => parse_cond(c),
            // ponytail: game/fomm/fose/nmm version checks always pass.
            _ => Cond::True,
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

pub fn parse(xml_bytes: &[u8]) -> crate::Result<ModuleConfig> {
    let xml = decode(xml_bytes);
    let doc = roxmltree::Document::parse_with_options(
        &xml,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
    .map_err(|e| format!("fomod/ModuleConfig.xml: {e}"))?;
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
        required: parse_files(child(root, "requiredInstallFiles")),
        steps,
        conditional,
    })
}

// ---------- evaluation ----------

type Flags = HashMap<String, String>;

/// `file_state(name)` returns Active | Inactive | Missing for a fileDependency.
fn holds(c: &Cond, flags: &Flags, file_state: &dyn Fn(&str) -> String) -> bool {
    match c {
        Cond::All(v) => v.iter().all(|c| holds(c, flags, file_state)),
        Cond::Any(v) => v.iter().any(|c| holds(c, flags, file_state)),
        Cond::Flag(f, v) => flags.get(f).map(String::as_str).unwrap_or("") == v,
        Cond::File(f, s) => file_state(f).eq_ignore_ascii_case(s),
        Cond::True => true,
    }
}

impl ModuleConfig {
    /// Walk steps in order; returns flags *before* each step, visibility, and final flags.
    fn walk(&self, sel: &Selection, fs: &dyn Fn(&str) -> String) -> (Vec<Flags>, Vec<bool>, Flags) {
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

    pub fn evaluate(&self, sel: &Selection, fs: &dyn Fn(&str) -> String) -> Eval {
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
    pub fn files(&self, sel: &Selection, fs: &dyn Fn(&str) -> String) -> Vec<FileItem> {
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
}

/// Shallowest `<dir>/fomod/ModuleConfig.xml` (case-insensitive) under `root`; returns (base dir, config path).
pub fn locate(root: &Path) -> Option<(PathBuf, PathBuf)> {
    walkdir::WalkDir::new(root)
        .sort_by_key(|e| e.depth())
        .into_iter()
        .filter_map(Result::ok)
        .find(|e| {
            e.file_name().eq_ignore_ascii_case("moduleconfig.xml")
                && e.path()
                    .parent()
                    .and_then(|p| p.file_name())
                    .is_some_and(|n| n.eq_ignore_ascii_case("fomod"))
        })
        .map(|e| {
            (
                e.path().parent().unwrap().parent().unwrap().to_path_buf(),
                e.into_path(),
            )
        })
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
        let fs = |f: &str| {
            if f == "Dawnguard.esm" {
                "Active".into()
            } else {
                "Missing".into()
            }
        };

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
}
