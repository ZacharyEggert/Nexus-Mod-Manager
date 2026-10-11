//! OBMM `.omod` archives (C# `Mod Formats/OMod`), converted in the staging folder into the layout
//! the other installers read: data files at the root, `fomod/script` (Mod Script),
//! `fomod/screenshot`, `fomod/config`.
//!
//! Packed `.omod` (an ordinary archive): `config` (binary metadata), `plugins` / `data`
//! (compressed streams of files end to end) with `plugins.crc` / `data.crc` listing them, and
//! `script`, `readme`, `image`. OMod-ready archive: an `omod conversion data/` folder holding
//! `config`, `script.txt`, `readme`, `screenshot` next to the mod's files.

use crate::{fomod::Info, Error, IoContext, Result, Task};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

const CONVERSION_FOLDER: &str = "omod conversion data";

/// C# `OModFormat.CheckFormatCompliance` + unpacking: rewrite an extracted OMod in `dir` into the
/// `fomod/` layout and return its metadata, or None when `dir` holds no OMod.
pub fn convert(dir: &Path, task: &Task) -> Result<Option<Info>> {
    let at = |n: &str| child_ci(dir, n);
    if let Some(config) = at("config").filter(|p| p.is_file()) {
        if ["plugins.crc", "data.crc", "image", "readme", "script"]
            .iter()
            .any(|n| at(n).is_some())
        {
            return convert_packed(dir, &config, task).map(Some);
        }
    }
    let Some(conv) = find_conversion(dir) else {
        return Ok(None);
    };
    let info = parse_config(&fs::read(conv.join("config")).at(&conv)?)?.0;
    let fomod = conv.parent().unwrap().join("fomod");
    // ponytail: an archive shipping both keeps its fomod/; files outside the OMod folder's
    // parent are left out (C# renames them into the root).
    if child_ci(conv.parent().unwrap(), "fomod").is_none() {
        fs::rename(&conv, &fomod).at(&conv)?;
        if let Some(s) = child_ci(&fomod, "script.txt") {
            let b = fs::read(&s).at(&s)?;
            fs::write(&s, trim_nul(&b)).at(&s)?;
        }
    }
    Ok(Some(info))
}

fn convert_packed(dir: &Path, config: &Path, task: &Task) -> Result<Info> {
    let (info, compression) = parse_config(&fs::read(config).at(config)?)?;
    let fomod = dir.join("fomod");
    fs::create_dir_all(&fomod).at(&fomod)?;
    // Move the OMod's own entries aside first so data files named like them can't collide.
    for (from, to) in [
        ("config", "config"),
        ("script", "script"),
        ("readme", "readme"),
        ("image", "screenshot"),
        ("plugins", "plugins"),
        ("plugins.crc", "plugins.crc"),
        ("data", "data"),
        ("data.crc", "data.crc"),
    ] {
        if let Some(p) = child_ci(dir, from) {
            let t = fomod.join(to);
            fs::rename(&p, &t).at(&p)?;
        }
    }
    // `script` / `readme` are one .NET string; Mod Scripts are read as ANSI.
    for n in ["script", "readme"] {
        let p = fomod.join(n);
        if p.exists() {
            let b = fs::read(&p).at(&p)?;
            let text = Reader(&b).string().unwrap_or_default();
            fs::write(&p, crate::ini::encode(text.trim_matches('\0'))).at(&p)?;
        }
    }
    for s in ["plugins", "data"] {
        let list = fomod.join(format!("{s}.crc"));
        if !list.exists() {
            continue;
        }
        let files = parse_crc(&fs::read(&list).at(&list)?)?;
        let stream = fomod.join(s);
        unpack(&stream, &files, compression, dir, task)?;
        fs::remove_file(&stream).at(&stream)?;
        fs::remove_file(&list).at(&list)?;
    }
    Ok(info)
}

/// Shallowest `omod conversion data/` folder holding a `config`.
fn find_conversion(dir: &Path) -> Option<PathBuf> {
    walkdir::WalkDir::new(dir)
        .sort_by(|a, b| a.depth().cmp(&b.depth()))
        .into_iter()
        .filter_map(Result::ok)
        .find(|e| {
            e.file_type().is_dir()
                && e.file_name().eq_ignore_ascii_case(CONVERSION_FOLDER)
                && child_ci(e.path(), "config").is_some()
        })
        .map(|e| e.into_path())
}

fn child_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().eq_ignore_ascii_case(name))
        .map(|e| e.path())
}

fn trim_nul(b: &[u8]) -> &[u8] {
    let end = b.iter().rposition(|&c| c != 0).map_or(0, |i| i + 1);
    &b[..end]
}

fn corrupt(what: impl std::fmt::Display) -> Error {
    Error::Other(format!("OMod is corrupt: {what}"))
}

/// .NET `BinaryReader` over a byte slice.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.0.len() < n {
            return Err(corrupt("unexpected end of data"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    /// 7-bit encoded length, then UTF-8 (`BinaryReader.ReadString`).
    fn string(&mut self) -> Result<String> {
        let (mut len, mut shift) = (0usize, 0);
        loop {
            let b = self.u8()?;
            len |= ((b & 0x7f) as usize) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 28 {
                return Err(corrupt("bad string length"));
            }
        }
        Ok(String::from_utf8_lossy(self.take(len)?).into_owned())
    }
}

/// C# `OMod.LoadInfo`: metadata and the stream compression (0 LZMA, 1 Zip).
fn parse_config(b: &[u8]) -> Result<(Info, u8)> {
    let mut r = Reader(b);
    let base = r.u8()?;
    let name = r.string()?;
    let major = r.i32()?;
    let minor = r.i32()?.max(0);
    let author = r.string()?;
    let _email = r.string()?;
    let _website = r.string()?;
    let _description = r.string()?;
    if base >= 2 {
        r.i64()?; // creation time
    } else {
        r.string()?;
    }
    let compression = r.u8()?;
    if compression > 1 {
        return Err(corrupt(format!("unknown compression type {compression}")));
    }
    let build = if base >= 1 { r.i32()? } else { -1 };
    // C# `Version.ToString()`: 2 components, or 4 when there is a build number.
    let version = if build < 0 {
        format!("{major}.{minor}")
    } else {
        format!("{major}.{minor}.{build}.0")
    };
    let mut id = String::new();
    if !r.0.is_empty() && r.u8()? >= 5 {
        id = r.string()?;
    }
    Ok((
        Info {
            name,
            author,
            version,
            id,
        },
        compression,
    ))
}

/// `plugins.crc` / `data.crc`: (name, CRC32, length) until the end.
fn parse_crc(b: &[u8]) -> Result<Vec<(String, u32, u64)>> {
    let mut r = Reader(b);
    let mut out = vec![];
    while !r.0.is_empty() {
        let name = r.string()?;
        let crc = r.u32()?;
        let len = u64::try_from(r.i64()?).map_err(|_| corrupt("negative file length"))?;
        out.push((name, crc, len));
    }
    Ok(out)
}

/// `name` under `dest`, refusing absolute paths and `..`.
fn safe_join(dest: &Path, name: &str) -> Result<PathBuf> {
    let mut p = dest.to_path_buf();
    for part in name
        .split(['\\', '/'])
        .filter(|s| !s.is_empty() && *s != ".")
    {
        if part == ".." || part.contains(':') {
            return Err(corrupt(format!("bad file name '{name}'")));
        }
        p.push(part);
    }
    if p == dest {
        return Err(corrupt("empty file name"));
    }
    Ok(p)
}

/// Decompress a file stream and split it into `files` under `dest`, checking each CRC.
fn unpack(
    stream: &Path,
    files: &[(String, u32, u64)],
    compression: u8,
    dest: &Path,
    task: &Task,
) -> Result<()> {
    let total: u64 = files.iter().map(|f| f.2).sum();
    let f = fs::File::open(stream).at(stream)?;
    let mut r: Box<dyn Read> = if compression == 0 {
        let mut f = BufReader::new(f);
        let mut props = [0u8; 5];
        f.read_exact(&mut props).at(stream)?;
        let dict = u32::from_le_bytes(props[1..].try_into().unwrap());
        Box::new(
            lzma_rust2::LzmaReader::new_with_props(f, total, props[0], dict, None)
                .map_err(corrupt)?,
        )
    } else {
        // ponytail: whole stream in memory, as C# does; stream it if huge OMods bite.
        let mut z = zip::ZipArchive::new(f).map_err(corrupt)?;
        let mut buf = vec![];
        z.by_index(0)
            .map_err(corrupt)?
            .read_to_end(&mut buf)
            .at(stream)?;
        Box::new(std::io::Cursor::new(buf))
    };
    let mut buf = vec![0u8; 64 * 1024];
    for (i, (name, crc, len)) in files.iter().enumerate() {
        task.step("Unpacking OMod", i as u64, files.len() as u64)?;
        let out = safe_join(dest, name)?;
        fs::create_dir_all(out.parent().unwrap()).at(&out)?;
        let mut w = fs::File::create(&out).at(&out)?;
        let mut h = crc32fast::Hasher::new();
        let mut left = *len;
        while left > 0 {
            let n = buf.len().min(left as usize);
            r.read_exact(&mut buf[..n])
                .map_err(|e| corrupt(format!("{name}: {e}")))?;
            h.update(&buf[..n]);
            w.write_all(&buf[..n]).at(&out)?;
            left -= n as u64;
        }
        if h.finalize() != *crc {
            return Err(corrupt(format!("{name}: checksums did not match")));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// .NET `BinaryWriter.Write(string)`.
    pub fn net_str(out: &mut Vec<u8>, s: &str) {
        let mut n = s.len();
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
        out.extend_from_slice(s.as_bytes());
    }

    pub fn config(name: &str, compression: u8) -> Vec<u8> {
        let mut c = vec![4u8]; // base version
        net_str(&mut c, name);
        c.extend(1i32.to_le_bytes());
        c.extend(2i32.to_le_bytes());
        net_str(&mut c, "Me");
        net_str(&mut c, "me@example.com");
        net_str(&mut c, "");
        net_str(&mut c, "desc");
        c.extend(0i64.to_le_bytes());
        c.push(compression);
        c.extend(3i32.to_le_bytes()); // build
        c.push(7); // OMod version
        net_str(&mut c, "1234");
        net_str(&mut c, "");
        net_str(&mut c, "");
        net_str(&mut c, "False");
        c.extend(0i32.to_le_bytes());
        c.extend(0i32.to_le_bytes());
        c
    }

    /// (`<s>.crc`, `<s>`) for `files`.
    pub fn stream(files: &[(&str, &[u8])], compression: u8) -> (Vec<u8>, Vec<u8>) {
        let mut crc = vec![];
        let mut all = vec![];
        for (n, b) in files {
            net_str(&mut crc, n);
            crc.extend(crc32fast::hash(b).to_le_bytes());
            crc.extend((b.len() as i64).to_le_bytes());
            all.extend_from_slice(b);
        }
        let data = if compression == 0 {
            let opts = lzma_rust2::LzmaOptions::with_preset(6);
            let mut out = vec![];
            let w = lzma_rust2::LzmaWriter::new_no_header(&mut out, &opts, false).unwrap();
            let props = w.props();
            let mut w = w;
            w.write_all(&all).unwrap();
            w.finish().unwrap();
            let mut s = vec![props];
            s.extend(opts.dict_size.to_le_bytes());
            s.extend(out);
            s
        } else {
            let mut z = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
            z.start_file("stream", zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(&all).unwrap();
            z.finish().unwrap().into_inner()
        };
        (crc, data)
    }

    fn script_entry(s: &str) -> Vec<u8> {
        let mut b = vec![];
        net_str(&mut b, s);
        b
    }

    fn packed(dir: &Path, compression: u8) {
        fs::write(dir.join("config"), config("Packed Mod", compression)).unwrap();
        let (crc, data) = stream(&[("Mine.esp", b"TES4")], compression);
        fs::write(dir.join("plugins.crc"), crc).unwrap();
        fs::write(dir.join("plugins"), data).unwrap();
        let (crc, data) = stream(
            &[
                ("meshes\\a.nif", b"nif"),
                ("textures/b.dds", b"dds!"),
                ("empty.txt", b""),
            ],
            compression,
        );
        fs::write(dir.join("data.crc"), crc).unwrap();
        fs::write(dir.join("data"), data).unwrap();
        fs::write(dir.join("script"), script_entry("CopyDataFile a b\0")).unwrap();
        fs::write(dir.join("image"), b"PNG").unwrap();
        fs::write(dir.join("readme"), script_entry("Read me")).unwrap();
    }

    #[test]
    fn packed_lzma_and_zip_unpack() {
        for compression in [0, 1] {
            let t = tempfile::tempdir().unwrap();
            packed(t.path(), compression);
            let info = convert(t.path(), &Task::default()).unwrap().unwrap();
            assert_eq!(
                info,
                Info {
                    name: "Packed Mod".into(),
                    author: "Me".into(),
                    version: "1.2.3.0".into(),
                    id: "1234".into(),
                }
            );
            let p = t.path();
            assert_eq!(fs::read(p.join("Mine.esp")).unwrap(), b"TES4");
            assert_eq!(fs::read(p.join("meshes/a.nif")).unwrap(), b"nif");
            assert_eq!(fs::read(p.join("textures/b.dds")).unwrap(), b"dds!");
            assert_eq!(fs::read(p.join("empty.txt")).unwrap(), b"");
            assert_eq!(
                fs::read_to_string(p.join("fomod/script")).unwrap(),
                "CopyDataFile a b"
            );
            assert_eq!(fs::read(p.join("fomod/screenshot")).unwrap(), b"PNG");
            for gone in ["config", "plugins", "data", "plugins.crc", "fomod/data"] {
                assert!(!p.join(gone).exists(), "{gone}");
            }
        }
    }

    #[test]
    fn bad_checksum_is_corrupt() {
        let t = tempfile::tempdir().unwrap();
        packed(t.path(), 0);
        let mut crc = vec![];
        net_str(&mut crc, "Mine.esp");
        crc.extend(0u32.to_le_bytes());
        crc.extend(4i64.to_le_bytes());
        fs::write(t.path().join("plugins.crc"), crc).unwrap();
        let e = convert(t.path(), &Task::default()).unwrap_err();
        assert!(e.to_string().contains("checksums"), "{e}");
    }

    #[test]
    fn escaping_names_refused() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("config"), config("X", 1)).unwrap();
        let (crc, data) = stream(&[("..\\evil.txt", b"x")], 1);
        fs::write(t.path().join("data.crc"), crc).unwrap();
        fs::write(t.path().join("data"), data).unwrap();
        assert!(convert(t.path(), &Task::default()).is_err());
        assert!(!t.path().parent().unwrap().join("evil.txt").exists());
    }

    #[test]
    fn old_config_versions() {
        // Base version 0: string creation time, no build, no OMod version.
        let mut c = vec![0u8];
        net_str(&mut c, "Old");
        c.extend(2i32.to_le_bytes());
        c.extend((-1i32).to_le_bytes());
        for s in ["A", "", "", "", "01/02/2006 10:00"] {
            net_str(&mut c, s);
        }
        c.push(0);
        let (info, comp) = parse_config(&c).unwrap();
        assert_eq!(
            (info.name.as_str(), info.version.as_str(), comp),
            ("Old", "2.0", 0)
        );
        assert!(info.id.is_empty());
    }

    #[test]
    fn omod_ready_folder_becomes_fomod() {
        let t = tempfile::tempdir().unwrap();
        let conv = t.path().join("Wrap/Omod Conversion Data");
        fs::create_dir_all(&conv).unwrap();
        fs::write(conv.join("config"), config("Ready", 0)).unwrap();
        fs::write(conv.join("script.txt"), "Message \"hi\"\0\0").unwrap();
        fs::write(t.path().join("Wrap/A.esp"), "").unwrap();
        let info = convert(t.path(), &Task::default()).unwrap().unwrap();
        assert_eq!(info.name, "Ready");
        assert_eq!(
            fs::read_to_string(t.path().join("Wrap/fomod/script.txt")).unwrap(),
            "Message \"hi\""
        );
        assert!(!conv.exists());
    }

    #[test]
    fn plain_archive_untouched() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("config"), "x").unwrap(); // no OMod markers
        fs::write(t.path().join("A.esp"), "").unwrap();
        assert!(convert(t.path(), &Task::default()).unwrap().is_none());
        assert!(t.path().join("config").exists());
    }
}
