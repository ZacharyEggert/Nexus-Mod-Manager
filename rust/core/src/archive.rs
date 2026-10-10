//! Extract mod archives (zip / 7z / rar) into a staging folder.

use crate::{Error, IoContext, Result, Task};
use std::path::Path;

const STEP: &str = "Extracting";

/// `archive.zip: <what the archive crate said>`.
fn bad(archive: &Path, e: impl std::fmt::Display) -> Error {
    Error::Other(format!("{}: {e}", archive.display()))
}

/// Extract per entry so `task` sees progress and can cancel between entries.
pub fn extract(archive: &Path, dest: &Path, task: &Task) -> Result<()> {
    std::fs::create_dir_all(dest).at(dest)?;
    let ext = archive
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "zip" | "fomod" => extract_zip(archive, dest, task),
        "7z" => extract_7z(archive, dest, task),
        "rar" => extract_rar(archive, dest, task),
        _ => Err(bad(archive, format!("unsupported archive type .{ext}"))),
    }
}

fn extract_zip(archive: &Path, dest: &Path, task: &Task) -> Result<()> {
    let f = std::fs::File::open(archive).at(archive)?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| bad(archive, e))?;
    let n = z.len() as u64;
    for i in 0..z.len() {
        task.step(STEP, i as u64, n)?;
        let mut e = z.by_index(i).map_err(|e| bad(archive, e))?;
        // enclosed_name rejects entries escaping `dest`; symlink entries are skipped outright.
        let Some(rel) = e.enclosed_name().filter(|_| !e.is_symlink()) else {
            continue;
        };
        let out = dest.join(rel);
        if e.is_dir() {
            std::fs::create_dir_all(&out).at(&out)?;
            continue;
        }
        let parent = out.parent().unwrap();
        std::fs::create_dir_all(parent).at(parent)?;
        let mut w = std::fs::File::create(&out).at(&out)?;
        std::io::copy(&mut e, &mut w).at(&out)?;
    }
    Ok(())
}

fn extract_7z(archive: &Path, dest: &Path, task: &Task) -> Result<()> {
    use sevenz_rust2::{default_entry_extract_fn, Archive, Error as SzError};
    let n = Archive::open(archive)
        .map_err(|e| bad(archive, e))?
        .files
        .len() as u64;
    let mut i = 0;
    // decompress_* joins entry names safely before calling us.
    let r = sevenz_rust2::decompress_file_with_extract_fn(archive, dest, |entry, reader, path| {
        task.step(STEP, i, n)
            .map_err(|e| SzError::Other(e.to_string().into()))?;
        i += 1;
        default_entry_extract_fn(entry, reader, path)
    });
    match r {
        Err(_) if task.cancelled() => Err(Error::Cancelled),
        r => r.map_err(|e| bad(archive, e)),
    }
}

fn extract_rar(archive: &Path, dest: &Path, task: &Task) -> Result<()> {
    let n = unrar::Archive::new(archive)
        .open_for_listing()
        .map_err(|e| bad(archive, e))?
        .count() as u64;
    let mut a = unrar::Archive::new(archive)
        .open_for_processing()
        .map_err(|e| bad(archive, e))?;
    let mut i = 0;
    while let Some(header) = a.read_header().map_err(|e| bad(archive, e))? {
        task.step(STEP, i, n)?;
        i += 1;
        a = if header.entry().is_file() {
            // unrar's own extractor sanitises paths relative to the base dir.
            header
                .extract_with_base(dest)
                .map_err(|e| bad(archive, e))?
        } else {
            header.skip().map_err(|e| bad(archive, e))?
        };
    }
    Ok(())
}
