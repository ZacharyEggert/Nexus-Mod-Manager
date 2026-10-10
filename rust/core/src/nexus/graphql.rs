//! Nexus GraphQL API v2.

use super::*;
use serde_json::json;

fn query(c: &Client, q: &str, vars: Value) -> Result<Value> {
    let req = c
        .http
        .post(format!("{}/v2/graphql", c.base))
        .json(&json!({ "query": q, "variables": vars }));
    let mut v = c.send(req)?;
    if let Some(e) = v["errors"].as_array().and_then(|e| e.first()) {
        let message = e["message"].as_str().unwrap_or("GraphQL error").to_string();
        let code = e["extensions"]["code"].as_str().unwrap_or("");
        return Err(match code {
            "UNAUTHORIZED" | "UNAUTHENTICATED" => Error::Unauthorized,
            c if c.ends_with("NOT_FOUND") => Error::Http {
                status: 404,
                message,
            },
            _ => Error::Http {
                status: 400,
                message,
            },
        });
    }
    Ok(v["data"].take())
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// BigInt fields arrive as strings.
fn num(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

const FILE_FIELDS: &str = "fileId name version category date sizeInBytes uri primary groupId";

fn file_info(v: &Value) -> FileInfo {
    FileInfo {
        file_id: num(&v["fileId"]).unwrap_or(0),
        name: s(&v["name"]),
        version: s(&v["version"]),
        category: s(&v["category"]).to_uppercase(),
        file_name: s(&v["uri"]),
        size: num(&v["sizeInBytes"]).unwrap_or(0),
        uploaded: v["date"].as_i64().unwrap_or(0),
        primary: v["primary"].as_u64().unwrap_or(0) != 0,
    }
}

pub fn game_id(c: &Client, domain: &str) -> Result<u64> {
    if let Some(&id) = c.game_ids.lock().unwrap().get(domain) {
        return Ok(id);
    }
    let d = query(
        c,
        "query($d: String) { game(domainName: $d) { id } }",
        json!({ "d": domain }),
    )?;
    let id = num(&d["game"]["id"]).ok_or_else(|| not_found("game"))?;
    c.game_ids.lock().unwrap().insert(domain.into(), id);
    Ok(id)
}

pub fn mod_info(c: &Client, domain: &str, mod_id: u64) -> Result<ModInfo> {
    let d = query(
        c,
        "query($ids: [CompositeDomainWithIdInput!]!) { legacyModsByDomain(ids: $ids) { nodes {
            modId gameId name version author summary modCategory { categoryId } viewerEndorsed } } }",
        json!({ "ids": [{ "gameDomain": domain, "modId": mod_id }] }),
    )?;
    let m = d["legacyModsByDomain"]["nodes"]
        .get(0)
        .ok_or_else(|| not_found("mod"))?;
    let game_id = num(&m["gameId"]).unwrap_or(0);
    c.game_ids.lock().unwrap().insert(domain.into(), game_id);
    Ok(ModInfo {
        mod_id: num(&m["modId"]).unwrap_or(mod_id),
        game_id,
        name: s(&m["name"]),
        version: s(&m["version"]),
        author: s(&m["author"]),
        summary: s(&m["summary"]),
        category_id: num(&m["modCategory"]["categoryId"]),
        endorsed: m["viewerEndorsed"].as_bool(),
    })
}

/// Files with their update group (versions of one file share a group).
pub fn files(c: &Client, domain: &str, mod_id: u64) -> Result<Vec<(FileInfo, Option<u64>)>> {
    let gid = game_id(c, domain)?;
    let d = query(
        c,
        &format!(
            "query($m: ID!, $g: ID!) {{ modFiles(modId: $m, gameId: $g) {{ {FILE_FIELDS} }} }}"
        ),
        json!({ "m": mod_id.to_string(), "g": gid.to_string() }),
    )?;
    Ok(d["modFiles"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| (file_info(f), num(&f["groupId"])))
        .collect())
}

pub fn by_md5(c: &Client, domain: &str, md5: &str) -> Result<Vec<HashMatch>> {
    let gid = game_id(c, domain)?;
    let d = query(
        c,
        &format!("query($h: String!) {{ fileHash(md5: $h) {{ gameId modFile {{ modId {FILE_FIELDS} }} }} }}"),
        json!({ "h": md5 }),
    )?;
    Ok(d["fileHash"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|h| num(&h["gameId"]) == Some(gid) && h["modFile"].is_object())
        .map(|h| HashMatch {
            mod_id: num(&h["modFile"]["modId"]).unwrap_or(0),
            file: file_info(&h["modFile"]),
        })
        .collect())
}

pub fn endorse(c: &Client, uid: u64, on: bool) -> Result<()> {
    let m = if on {
        "createModEndorsement"
    } else {
        "abstainFromModEndorsement"
    };
    let d = query(
        c,
        &format!("mutation($u: String!) {{ {m}(modUid: $u) {{ success }} }}"),
        json!({ "u": uid.to_string() }),
    )?;
    if d[m]["success"].as_bool() == Some(true) {
        Ok(())
    } else {
        Err(Error::Other(format!("Nexus: {m} failed")))
    }
}
