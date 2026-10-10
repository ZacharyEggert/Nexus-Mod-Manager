//! Nexus REST API v1.

use super::*;

fn get(c: &Client, path: &str) -> Result<Value> {
    c.send(c.http.get(format!("{}/v1/{path}", c.base)))
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn file_info(v: &Value) -> FileInfo {
    FileInfo {
        file_id: v["file_id"].as_u64().unwrap_or(0),
        name: s(&v["name"]),
        version: s(&v["version"]),
        category: s(&v["category_name"]).to_uppercase(),
        file_name: s(&v["file_name"]),
        size: v["size_in_bytes"]
            .as_u64()
            .or(v["size_kb"].as_u64().map(|k| k * 1024))
            .unwrap_or(0),
        uploaded: v["uploaded_timestamp"].as_i64().unwrap_or(0),
        primary: v["is_primary"].as_bool().unwrap_or(false),
    }
}

pub fn validate(c: &Client) -> Result<User> {
    let v = get(c, "users/validate.json")?;
    Ok(User {
        user_id: v["user_id"].as_u64().unwrap_or(0),
        name: s(&v["name"]),
        premium: v["is_premium"].as_bool().unwrap_or(false),
        supporter: v["is_supporter"].as_bool().unwrap_or(false),
    })
}

pub fn mod_info(c: &Client, domain: &str, mod_id: u64) -> Result<ModInfo> {
    let v = get(c, &format!("games/{domain}/mods/{mod_id}.json"))?;
    Ok(ModInfo {
        mod_id: v["mod_id"].as_u64().unwrap_or(mod_id),
        game_id: v["game_id"].as_u64().unwrap_or(0),
        name: s(&v["name"]),
        version: s(&v["version"]),
        author: s(&v["author"]),
        summary: s(&v["summary"]),
        category_id: v["category_id"].as_u64(),
        endorsed: match v["endorsement"]["endorse_status"].as_str() {
            Some("Endorsed") => Some(true),
            Some("Abstained") => Some(false),
            _ => None,
        },
    })
}

/// `(old, new)` file replacement pairs.
type Updates = Vec<(u64, u64)>;

/// Files plus their replacement pairs.
pub fn files(c: &Client, domain: &str, mod_id: u64) -> Result<(Vec<FileInfo>, Updates)> {
    let v = get(c, &format!("games/{domain}/mods/{mod_id}/files.json"))?;
    let list = |k: &str| v[k].as_array().cloned().unwrap_or_default();
    Ok((
        list("files").iter().map(file_info).collect(),
        list("file_updates")
            .iter()
            .map(|u| {
                (
                    u["old_file_id"].as_u64().unwrap_or(0),
                    u["new_file_id"].as_u64().unwrap_or(0),
                )
            })
            .collect(),
    ))
}

pub fn file(c: &Client, domain: &str, mod_id: u64, file_id: u64) -> Result<FileInfo> {
    Ok(file_info(&get(
        c,
        &format!("games/{domain}/mods/{mod_id}/files/{file_id}.json"),
    )?))
}

pub fn by_md5(c: &Client, domain: &str, md5: &str) -> Result<Vec<HashMatch>> {
    let v = match get(c, &format!("games/{domain}/mods/md5_search/{md5}.json")) {
        // v1 answers "no match" with 404.
        Err(Error::Http { status: 404, .. }) => return Ok(vec![]),
        r => r?,
    };
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .map(|h| HashMatch {
            mod_id: h["mod"]["mod_id"].as_u64().unwrap_or(0),
            file: file_info(&h["file_details"]),
        })
        .collect())
}

pub fn download_links(
    c: &Client,
    domain: &str,
    mod_id: u64,
    file_id: u64,
    key: Option<&NxmKey>,
) -> Result<Vec<DownloadLink>> {
    let mut path = format!("games/{domain}/mods/{mod_id}/files/{file_id}/download_link.json");
    if let Some(k) = key {
        path += &format!("?key={}&expires={}", k.key, k.expires);
    }
    let v = get(c, &path)?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .map(|l| DownloadLink {
            name: s(&l["name"]),
            short_name: s(&l["short_name"]),
            url: s(&l["URI"]),
        })
        .collect())
}

pub fn updated(c: &Client, domain: &str, period: Period) -> Result<Vec<u64>> {
    let p = match period {
        Period::Day => "1d",
        Period::Week => "1w",
        Period::Month => "1m",
    };
    let v = get(c, &format!("games/{domain}/mods/updated.json?period={p}"))?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["mod_id"].as_u64())
        .collect())
}

pub fn endorse(c: &Client, domain: &str, mod_id: u64, version: &str, on: bool) -> Result<()> {
    let action = if on { "endorse" } else { "abstain" };
    let url = format!("{}/v1/games/{domain}/mods/{mod_id}/{action}.json", c.base);
    c.send(c.http.post(url).form(&[("version", version)]))?;
    Ok(())
}

pub fn categories(c: &Client, domain: &str) -> Result<Vec<Category>> {
    let v = get(c, &format!("games/{domain}.json"))?;
    Ok(v["categories"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| Category {
            id: c["category_id"].as_u64().unwrap_or(0),
            name: s(&c["name"]),
            parent: c["parent_category"].as_u64(),
        })
        .collect())
}
