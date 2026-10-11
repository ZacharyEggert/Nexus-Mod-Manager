//! Bundle every `Game Modes/Definitions/<id>/`: game.json, Categories.xml, and when present the
//! base-files list (`resources.baseFilesPath`) and supportedTools.json.

use std::fmt::Write;
use std::path::Path;

fn main() {
    let defs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Game Modes/Definitions");
    let defs = defs.canonicalize().expect("Game Modes/Definitions");
    println!("cargo:rerun-if-changed={}", defs.display());
    let mut dirs: Vec<_> = std::fs::read_dir(&defs)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("game.json").is_file())
        .collect();
    dirs.sort();
    let lit = |p: &Path| format!("include_str!({:?})", p.display().to_string());
    let opt = |p: &Path| match p.is_file() {
        true => format!("Some({})", lit(p)),
        false => "None".into(),
    };
    let mut out = String::from("pub(crate) const BUNDLED: &[Bundle] = &[\n");
    for d in dirs {
        println!("cargo:rerun-if-changed={}", d.display());
        let json = std::fs::read_to_string(d.join("game.json")).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(json.trim_start_matches('\u{feff}')).unwrap();
        let base = v["resources"]["baseFilesPath"].as_str().map(|b| d.join(b));
        writeln!(
            out,
            "    Bundle {{ game: {}, categories: {}, base_files: {}, tools: {} }},",
            lit(&d.join("game.json")),
            lit(&d.join("Categories.xml")),
            base.as_deref().map_or("None".into(), opt),
            opt(&d.join("supportedTools.json")),
        )
        .unwrap();
    }
    out.push_str("];\n");
    let dest = Path::new(&std::env::var("OUT_DIR").unwrap()).join("bundled.rs");
    std::fs::write(dest, out).unwrap();
}
