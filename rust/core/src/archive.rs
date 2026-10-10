//! Extract mod archives (zip / 7z / rar) into a staging folder.

use std::path::Path;

pub fn extract(archive: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let ext = archive.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "zip" | "fomod" => {
            let f = std::fs::File::open(archive).map_err(|e| e.to_string())?;
            // ZipArchive::extract rejects entries escaping `dest`.
            zip::ZipArchive::new(f).and_then(|mut z| z.extract(dest)).map_err(|e| e.to_string())
        }
        "7z" => sevenz_rust2::decompress_file(archive, dest).map_err(|e| e.to_string()),
        "rar" => extract_rar(archive, dest),
        _ => Err(format!("unsupported archive type: .{ext}")),
    }
}

fn extract_rar(archive: &Path, dest: &Path) -> Result<(), String> {
    let mut a = unrar::Archive::new(archive).open_for_processing().map_err(|e| e.to_string())?;
    while let Some(header) = a.read_header().map_err(|e| e.to_string())? {
        a = if header.entry().is_file() {
            // unrar's own extractor sanitises paths relative to the base dir.
            header.extract_with_base(dest).map_err(|e| e.to_string())?
        } else {
            header.skip().map_err(|e| e.to_string())?
        };
    }
    Ok(())
}
