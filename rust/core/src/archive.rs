//! Extract mod archives (zip / 7z / rar) into a staging folder.

use crate::Task;
use std::path::Path;

const STEP: &str = "Extracting";

/// Extract per entry so `task` sees progress and can cancel between entries.
pub fn extract(archive: &Path, dest: &Path, task: &Task) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let ext = archive.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "zip" | "fomod" => extract_zip(archive, dest, task),
        "7z" => extract_7z(archive, dest, task),
        "rar" => extract_rar(archive, dest, task),
        _ => Err(format!("unsupported archive type: .{ext}")),
    }
}

fn extract_zip(archive: &Path, dest: &Path, task: &Task) -> Result<(), String> {
    let f = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
    let n = z.len() as u64;
    for i in 0..z.len() {
        task.step(STEP, i as u64, n)?;
        let mut e = z.by_index(i).map_err(|e| e.to_string())?;
        // enclosed_name rejects entries escaping `dest`; symlink entries are skipped outright.
        let Some(rel) = e.enclosed_name().filter(|_| !e.is_symlink()) else { continue };
        let out = dest.join(rel);
        if e.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        std::fs::create_dir_all(out.parent().unwrap()).map_err(|e| e.to_string())?;
        let mut w = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut e, &mut w).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    Ok(())
}

fn extract_7z(archive: &Path, dest: &Path, task: &Task) -> Result<(), String> {
    use sevenz_rust2::{default_entry_extract_fn, Archive, Error};
    let n = Archive::open(archive).map_err(|e| e.to_string())?.files.len() as u64;
    let mut i = 0;
    // decompress_* joins entry names safely before calling us.
    let r = sevenz_rust2::decompress_file_with_extract_fn(archive, dest, |entry, reader, path| {
        task.step(STEP, i, n).map_err(|e| Error::Other(e.into()))?;
        i += 1;
        default_entry_extract_fn(entry, reader, path)
    });
    match r {
        Err(_) if task.cancelled() => Err(Task::CANCELLED.into()),
        r => r.map_err(|e| e.to_string()),
    }
}

fn extract_rar(archive: &Path, dest: &Path, task: &Task) -> Result<(), String> {
    let n = unrar::Archive::new(archive).open_for_listing().map_err(|e| e.to_string())?.count() as u64;
    let mut a = unrar::Archive::new(archive).open_for_processing().map_err(|e| e.to_string())?;
    let mut i = 0;
    while let Some(header) = a.read_header().map_err(|e| e.to_string())? {
        task.step(STEP, i, n)?;
        i += 1;
        a = if header.entry().is_file() {
            // unrar's own extractor sanitises paths relative to the base dir.
            header.extract_with_base(dest).map_err(|e| e.to_string())?
        } else {
            header.skip().map_err(|e| e.to_string())?
        };
    }
    Ok(())
}
