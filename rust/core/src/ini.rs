//! Minimal INI read/write with Windows `Get/WritePrivateProfileString` semantics: sections and
//! keys match case-insensitively, a write replaces the key's line in place (or adds it to the end
//! of its section, or adds the section). Bytes outside the edited line are kept as they were.

use crate::{write_durable, IoContext, Result};
use std::path::Path;

/// INI files are ANSI; Latin-1 round-trips every byte.
pub(crate) fn decode(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

pub(crate) fn encode(s: &str) -> Vec<u8> {
    s.chars().map(|c| c as u32 as u8).collect()
}

fn section_of(line: &str) -> Option<&str> {
    line.trim()
        .strip_prefix('[')?
        .split_once(']')
        .map(|(s, _)| s.trim())
}

pub(crate) fn key_of(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with(';') || t.starts_with('#') {
        return None;
    }
    Some(t.split_once('=')?.0.trim())
}

pub fn get_in(text: &str, section: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for line in text.lines() {
        if let Some(s) = section_of(line) {
            inside = s.eq_ignore_ascii_case(section);
        } else if inside && key_of(line).is_some_and(|k| k.eq_ignore_ascii_case(key)) {
            let v = line.split_once('=').unwrap().1.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(v);
            return Some(v.to_string());
        }
    }
    None
}

pub fn set_in(text: &str, section: &str, key: &str, value: &str) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let entry = format!("{key}={value}");
    let mut inside = false;
    // Index to insert after: last non-blank line of the section.
    let mut last: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(s) = section_of(line) {
            if inside {
                break;
            }
            inside = s.eq_ignore_ascii_case(section);
            if inside {
                last = Some(i);
            }
        } else if inside {
            if key_of(line).is_some_and(|k| k.eq_ignore_ascii_case(key)) {
                lines[i] = entry;
                return lines.join(nl) + nl;
            }
            if !line.trim().is_empty() {
                last = Some(i);
            }
        }
    }
    match last {
        Some(i) => lines.insert(i + 1, entry),
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("[{section}]"));
            lines.push(entry);
        }
    }
    lines.join(nl) + nl
}

/// `prefix<N>` key of a line (`GameFile3=` -> true for `GameFile`).
fn numbered(line: &str, prefix: &str) -> bool {
    key_of(line).is_some_and(|k| {
        k.len() > prefix.len()
            && k[..prefix.len()].eq_ignore_ascii_case(prefix)
            && k[prefix.len()..].bytes().all(|b| b.is_ascii_digit())
    })
}

/// Values of `section`'s `prefix<N>=` keys in file order (Morrowind `[Game Files]` `GameFile0=`).
pub(crate) fn numbered_in(text: &str, section: &str, prefix: &str) -> Vec<String> {
    let mut inside = false;
    let mut out = vec![];
    for line in text.lines() {
        if let Some(s) = section_of(line) {
            inside = s.eq_ignore_ascii_case(section);
        } else if inside && numbered(line, prefix) {
            out.push(line.split_once('=').unwrap().1.trim().to_string());
        }
    }
    out
}

/// `text` with `section`'s `prefix<N>` keys replaced by `values` numbered from 0, where the first
/// one was (else after the section's last line; the section is added when missing).
pub(crate) fn set_numbered_in(text: &str, section: &str, prefix: &str, values: &[&str]) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let new = values
        .iter()
        .enumerate()
        .map(|(i, v)| format!("{prefix}{i}={v}"));
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let Some(start) = lines
        .iter()
        .position(|l| section_of(l).is_some_and(|s| s.eq_ignore_ascii_case(section)))
    else {
        if lines.last().is_some_and(|l| !l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(format!("[{section}]"));
        lines.extend(new);
        return lines.join(nl) + nl;
    };
    let end = (start + 1..lines.len())
        .find(|&i| section_of(&lines[i]).is_some())
        .unwrap_or(lines.len());
    let at = (start + 1..end)
        .find(|&i| numbered(&lines[i], prefix))
        .unwrap_or_else(|| {
            (start + 1..end)
                .rev()
                .find(|&i| !lines[i].trim().is_empty())
                .map_or(start + 1, |i| i + 1)
        });
    let mut out: Vec<String> = vec![];
    let mut new = Some(new);
    for (i, l) in lines.into_iter().enumerate() {
        if i == at {
            out.extend(new.take().into_iter().flatten());
        }
        if !(i > start && i < end && numbered(&l, prefix)) {
            out.push(l);
        }
    }
    out.extend(new.into_iter().flatten());
    out.join(nl) + nl
}

pub fn get(path: &Path, section: &str, key: &str) -> Option<String> {
    get_in(&decode(&std::fs::read(path).ok()?), section, key)
}

pub fn set(path: &Path, section: &str, key: &str, value: &str) -> Result<()> {
    let text = decode(&std::fs::read(path).at(path)?);
    let new = set_in(&text, section, key, value);
    if new != text {
        write_durable(path, &encode(&new))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_edits_like_windows() {
        let text = "[General]\r\nsLanguage=ENGLISH\r\n\r\n[archive]\r\n; comment=x\r\nSArchiveList = a.bsa, b.bsa\r\nbInvalidateOlderFiles=0\r\n\r\n[Display]\r\niSize W=1280\r\n";
        assert_eq!(
            get_in(text, "Archive", "sarchivelist").as_deref(),
            Some("a.bsa, b.bsa")
        );
        assert_eq!(get_in(text, "Archive", "comment"), None);
        assert_eq!(get_in(text, "Display", "nothing"), None);
        // Replace in place, keeping the rest byte for byte.
        let t = set_in(text, "Archive", "bInvalidateOlderFiles", "1");
        assert_eq!(
            t,
            text.replace("bInvalidateOlderFiles=0", "bInvalidateOlderFiles=1")
        );
        // New key: end of its section, before the blank line.
        let t = set_in(text, "ARCHIVE", "SInvalidationFile", "");
        assert!(
            t.contains("bInvalidateOlderFiles=0\r\nSInvalidationFile=\r\n\r\n[Display]"),
            "{t}"
        );
        // New section at the end.
        let t = set_in("[A]\nx=1", "General", "bLoad", "1");
        assert_eq!(t, "[A]\nx=1\n\n[General]\nbLoad=1\n");
        // Numbered keys (Morrowind [Game Files]): replaced where the first one was.
        let mw = "[Game Files]\r\nGameFile0=Morrowind.esm\r\nGameFile1=Old.esp\r\n\r\n[General]\r\nx=1\r\n";
        assert_eq!(
            numbered_in(mw, "game files", "GameFile"),
            ["Morrowind.esm", "Old.esp"]
        );
        assert_eq!(
            set_numbered_in(
                mw,
                "Game Files",
                "GameFile",
                &["Morrowind.esm", "A.esp", "B.esp"]
            ),
            mw.replace("GameFile1=Old.esp", "GameFile1=A.esp\r\nGameFile2=B.esp")
        );
        assert_eq!(
            set_numbered_in(
                "[Game Files]\n\n[X]\n",
                "Game Files",
                "GameFile",
                &["M.esm"]
            ),
            "[Game Files]\nGameFile0=M.esm\n\n[X]\n"
        );
        assert_eq!(
            set_numbered_in("[X]\ny=1", "Game Files", "GameFile", &["M.esm"]),
            "[X]\ny=1\n\n[Game Files]\nGameFile0=M.esm\n"
        );
        // Non-UTF-8 bytes survive.
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("x.ini");
        std::fs::write(&p, b"[A]\r\nname=caf\xe9\r\nk=0\r\n").unwrap();
        set(&p, "A", "k", "1").unwrap();
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"[A]\r\nname=caf\xe9\r\nk=1\r\n"
        );
        assert_eq!(get(&p, "a", "NAME").as_deref(), Some("caf\u{e9}"));
    }
}
