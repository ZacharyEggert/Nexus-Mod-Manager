//! Nexus links: `nxm://` download links and nexusmods.com mod pages (port of C#
//! `NexusModLinkParser` + `NexusUrl`).

use super::NxmKey;
use reqwest::Url;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    /// Lower case, as written in the link (`skyrimspecialedition`, legacy `skyrimse`).
    pub domain: String,
    pub mod_id: u64,
    pub file_id: Option<u64>,
    /// Download key (nxm links from the site's "Mod manager download" button).
    pub key: Option<NxmKey>,
    pub user_id: Option<u64>,
    /// The original text.
    pub source: String,
}

/// Positive integer ids only, like C# `IsValidId`.
fn id(s: &str) -> Option<u64> {
    s.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| s.parse().ok())
        .flatten()
        .filter(|&n| n > 0)
}

/// Parse an `nxm://` link or a nexusmods.com mod page; `www.` links may omit the scheme.
pub fn parse(text: &str) -> Option<Link> {
    let t = text.trim();
    let url = Url::parse(t)
        .ok()
        .filter(|u| u.has_host())
        .or_else(|| Url::parse(&format!("https://{t}")).ok())?;
    let host = url.host_str()?.to_ascii_lowercase();
    let segs: Vec<String> = url
        .path_segments()?
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect();
    let seg = |i: usize| segs.get(i).map(String::as_str);
    let q = |k: &str| {
        url.query_pairs()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.into_owned())
    };
    let (domain, mod_id, file_id) = match url.scheme().to_ascii_lowercase().as_str() {
        "nxm" => {
            if !seg(0)?.eq_ignore_ascii_case("mods") {
                return None;
            }
            let file = seg(2)
                .filter(|s| s.eq_ignore_ascii_case("files"))
                .and_then(|_| id(seg(3)?));
            (host, id(seg(1)?)?, file)
        }
        "http" | "https" => {
            let file = q("file_id").as_deref().and_then(id);
            if host == "nexusmods.com" || host == "www.nexusmods.com" {
                if !seg(1)?.eq_ignore_ascii_case("mods") {
                    return None;
                }
                (seg(0)?.to_ascii_lowercase(), id(seg(2)?)?, file)
            } else {
                // Legacy subdomain links: skyrim.nexusmods.com/mods/321
                let domain = host.strip_suffix(".nexusmods.com")?.to_string();
                if domain.is_empty() || !seg(0)?.eq_ignore_ascii_case("mods") {
                    return None;
                }
                (domain, id(seg(1)?)?, file)
            }
        }
        _ => return None,
    };
    let key = match (q("key"), q("expires").and_then(|e| e.parse().ok())) {
        (Some(key), Some(expires)) if !key.is_empty() => Some(NxmKey { key, expires }),
        _ => None,
    };
    Some(Link {
        domain,
        mod_id,
        file_id,
        key,
        user_id: q("user_id").and_then(|u| u.parse().ok()),
        source: t.into(),
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = || u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok();
        match (b[i], hex()) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into()
}

/// Mod page (or its file tab), like C# `CreateModUri`.
pub fn mod_url(domain: &str, mod_id: u64, file_id: Option<u64>) -> String {
    let d = domain.trim().trim_matches('/').to_ascii_lowercase();
    match file_id {
        Some(f) => format!("https://www.nexusmods.com/{d}/mods/{mod_id}?tab=files&file_id={f}"),
        None => format!("https://www.nexusmods.com/{d}/mods/{mod_id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cases from C# NexusModLinkParserTests, plus key/expiry parsing from NexusUrl.
    #[test]
    fn parses_links_like_csharp() {
        let l = parse("https://www.nexusmods.com/skyrimspecialedition/mods/12345").unwrap();
        assert_eq!(
            (l.domain.as_str(), l.mod_id, l.file_id),
            ("skyrimspecialedition", 12345, None)
        );
        let l = parse("https://www.nexusmods.com/fallout4/mods/100?tab=files&file_id=200").unwrap();
        assert_eq!((l.mod_id, l.file_id), (100, Some(200)));
        let l = parse("https://skyrim.nexusmods.com/mods/321").unwrap();
        assert_eq!((l.domain.as_str(), l.mod_id), ("skyrim", 321));
        let l = parse("nxm://fallout4/mods/100/files/200").unwrap();
        assert_eq!(
            (l.domain.as_str(), l.mod_id, l.file_id, l.key),
            ("fallout4", 100, Some(200), None)
        );
        let l = parse("www.nexusmods.com/skyrim/mods/42").unwrap();
        assert_eq!((l.domain.as_str(), l.mod_id), ("skyrim", 42));
        // Site download button: host keeps case-insensitivity, key/expires/user_id parsed.
        let l = parse("nxm://SkyrimSpecialEdition/mods/12604/files/749043?key=a-B_c&expires=1791673969&user_id=9").unwrap();
        assert_eq!(l.domain, "skyrimspecialedition");
        assert_eq!(
            l.key,
            Some(NxmKey {
                key: "a-B_c".into(),
                expires: 1791673969
            })
        );
        assert_eq!(l.user_id, Some(9));
    }

    #[test]
    fn rejects_non_mod_links() {
        for bad in [
            "",
            "https://example.com/mods/1",
            "https://www.nexusmods.com/skyrim/collections/1",
            "nxm://skyrim/mods/0/files/1",
            "nxm://skyrim/mods/-1",
            "nxm://skyrim/profiles/3",
            "ftp://www.nexusmods.com/skyrim/mods/1",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        // Bad file id: still a mod link, without a file.
        assert_eq!(parse("nxm://skyrim/mods/5/files/x").unwrap().file_id, None);
    }

    #[test]
    fn builds_mod_urls() {
        assert_eq!(
            mod_url("Fallout4", 100, Some(200)),
            "https://www.nexusmods.com/fallout4/mods/100?tab=files&file_id=200"
        );
        assert_eq!(
            mod_url("fallout4", 100, None),
            "https://www.nexusmods.com/fallout4/mods/100"
        );
    }
}
