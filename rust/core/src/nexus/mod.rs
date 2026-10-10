//! Nexus Mods API client: GraphQL (v2) by default, REST (v1) as a setting. Operations GraphQL
//! lacks (download links, key validation, updated-mods list, categories) always go to v1.

mod graphql;
pub mod link;
mod rest;
pub mod sso;
pub mod updates;

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Which API serves the operations both can answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Api {
    #[default]
    Graphql,
    Rest,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct User {
    pub user_id: u64,
    pub name: String,
    pub premium: bool,
    pub supporter: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModInfo {
    pub mod_id: u64,
    pub game_id: u64,
    pub name: String,
    pub version: String,
    pub author: String,
    pub summary: String,
    pub category_id: Option<u64>,
    /// `None` when unknown (not logged in, or never endorsed/abstained).
    pub endorsed: Option<bool>,
}

impl ModInfo {
    /// Site-wide mod id, as GraphQL mutations want it.
    pub fn uid(&self) -> u64 {
        self.game_id << 32 | self.mod_id
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileInfo {
    pub file_id: u64,
    pub name: String,
    pub version: String,
    /// Upper case, as v1 names it: MAIN, UPDATE, OPTIONAL, OLD_VERSION, MISCELLANEOUS, ARCHIVED, ...
    pub category: String,
    pub file_name: String,
    pub size: u64,
    /// Unix seconds.
    pub uploaded: i64,
    pub primary: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HashMatch {
    pub mod_id: u64,
    pub file: FileInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DownloadLink {
    pub name: String,
    pub short_name: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Category {
    pub id: u64,
    pub name: String,
    pub parent: Option<u64>,
}

/// Window for `Client::updated`.
#[derive(Debug, Clone, Copy)]
pub enum Period {
    Day,
    Week,
    Month,
}

/// Last `X-RL-*` headers seen.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RateLimit {
    pub hourly_remaining: u32,
    pub hourly_limit: u32,
    pub hourly_reset: String,
    pub daily_remaining: u32,
    pub daily_limit: u32,
    pub daily_reset: String,
}

/// Download key from an `nxm://` link; non-premium users need it to get download links.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NxmKey {
    pub key: String,
    pub expires: u64,
}

pub const BASE: &str = "https://api.nexusmods.com";
// ponytail: own identity until the Nexus app registration lands (SSO slug, #13).
const APP_NAME: &str = "NexusModManagerRs";
/// After a 429 (or both quotas at 0), requests fail locally for this long instead of hitting Nexus.
const BACKOFF: Duration = Duration::from_secs(300);

pub struct Client {
    http: reqwest::blocking::Client,
    key: String,
    pub api: Api,
    base: String,
    rate: Mutex<Option<RateLimit>>,
    blocked_until: Mutex<Option<Instant>>,
    /// domain -> numeric game id (GraphQL wants the id).
    game_ids: Mutex<HashMap<String, u64>>,
}

impl Client {
    pub fn new(key: &str, api: Api) -> Self {
        Self::with_base(key, api, BASE)
    }

    /// `base` replaces `https://api.nexusmods.com` (tests point it at a local server).
    pub fn with_base(key: &str, api: Api, base: &str) -> Self {
        let ua = format!(
            "{APP_NAME}/{} ({}; {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        Self {
            http: reqwest::blocking::Client::builder()
                .user_agent(ua)
                .timeout(Duration::from_secs(30))
                .build()
                .expect("http client"),
            key: key.trim().into(),
            api,
            base: base.trim_end_matches('/').into(),
            rate: Mutex::default(),
            blocked_until: Mutex::default(),
            game_ids: Mutex::default(),
        }
    }

    pub fn rate_limit(&self) -> Option<RateLimit> {
        self.rate.lock().unwrap().clone()
    }

    /// Checks the key; v1 only (GraphQL has no "who am I").
    pub fn validate(&self) -> Result<User> {
        rest::validate(self)
    }

    pub fn mod_info(&self, domain: &str, mod_id: u64) -> Result<ModInfo> {
        match self.api {
            Api::Graphql => graphql::mod_info(self, domain, mod_id),
            Api::Rest => rest::mod_info(self, domain, mod_id),
        }
    }

    /// Every file of a mod, old and archived ones included.
    pub fn files(&self, domain: &str, mod_id: u64) -> Result<Vec<FileInfo>> {
        match self.api {
            Api::Graphql => Ok(graphql::files(self, domain, mod_id)?
                .into_iter()
                .map(|(f, _)| f)
                .collect()),
            Api::Rest => Ok(rest::files(self, domain, mod_id)?.0),
        }
    }

    pub fn file(&self, domain: &str, mod_id: u64, file_id: u64) -> Result<FileInfo> {
        match self.api {
            Api::Graphql => graphql::files(self, domain, mod_id)?
                .into_iter()
                .map(|(f, _)| f)
                .find(|f| f.file_id == file_id)
                .ok_or_else(|| not_found("file")),
            Api::Rest => rest::file(self, domain, mod_id, file_id),
        }
    }

    /// The file a mod-page link means: newest MAIN file, else newest of any (C# `GetDefaultFileInfo`).
    pub fn default_file(&self, domain: &str, mod_id: u64) -> Result<FileInfo> {
        let files = self.files(domain, mod_id)?;
        let newest = |main: bool| {
            files
                .iter()
                .filter(|f| !main || f.category == "MAIN")
                .max_by_key(|f| (f.uploaded, f.file_id))
                .cloned()
        };
        newest(true)
            .or_else(|| newest(false))
            .ok_or_else(|| not_found("file"))
    }

    /// Newest file in `file_id`'s update chain (itself when nothing replaced it).
    pub fn latest(&self, domain: &str, mod_id: u64, file_id: u64) -> Result<FileInfo> {
        match self.api {
            Api::Graphql => {
                let files = graphql::files(self, domain, mod_id)?;
                let group = files
                    .iter()
                    .find(|(f, _)| f.file_id == file_id)
                    .ok_or_else(|| not_found("file"))?
                    .1;
                Ok(files
                    .into_iter()
                    .filter(|(f, g)| f.file_id == file_id || (group.is_some() && *g == group))
                    .map(|(f, _)| f)
                    .max_by_key(|f| (f.uploaded, f.file_id))
                    .unwrap())
            }
            Api::Rest => {
                let (files, updates) = rest::files(self, domain, mod_id)?;
                let id = follow_chain(file_id, &updates);
                let get = |id| match files.iter().find(|f| f.file_id == id) {
                    Some(f) => Ok(f.clone()),
                    None => rest::file(self, domain, mod_id, id),
                };
                let (start, end) = (get(file_id)?, get(id)?);
                // Nexus has chains pointing at older uploads; never call those an update.
                Ok(if end.uploaded < start.uploaded {
                    start
                } else {
                    end
                })
            }
        }
    }

    /// Files whose archive has this md5, in `domain`.
    pub fn by_md5(&self, domain: &str, md5: &str) -> Result<Vec<HashMatch>> {
        match self.api {
            Api::Graphql => graphql::by_md5(self, domain, md5),
            Api::Rest => rest::by_md5(self, domain, md5),
        }
    }

    /// CDN links for a file; v1 only. Non-premium users must pass the key from the nxm link.
    pub fn download_links(
        &self,
        domain: &str,
        mod_id: u64,
        file_id: u64,
        key: Option<&NxmKey>,
    ) -> Result<Vec<DownloadLink>> {
        rest::download_links(self, domain, mod_id, file_id, key)
    }

    /// Ids of mods updated within `period`; v1 only.
    pub fn updated(&self, domain: &str, period: Period) -> Result<Vec<u64>> {
        rest::updated(self, domain, period)
    }

    /// Endorse (`on`) or abstain.
    pub fn endorse(&self, domain: &str, mod_id: u64, version: &str, on: bool) -> Result<()> {
        match self.api {
            Api::Graphql => {
                let uid = graphql::game_id(self, domain)? << 32 | mod_id;
                graphql::endorse(self, uid, on)
            }
            Api::Rest => rest::endorse(self, domain, mod_id, version, on),
        }
    }

    /// Mod categories of a game; v1 only (GraphQL `categories` is the newer, different taxonomy).
    pub fn categories(&self, domain: &str) -> Result<Vec<Category>> {
        rest::categories(self, domain)
    }

    /// Send with auth headers, track rate limits, map HTTP errors, parse JSON.
    fn send(&self, req: reqwest::blocking::RequestBuilder) -> Result<Value> {
        if let Some(t) = *self.blocked_until.lock().unwrap() {
            if Instant::now() < t {
                return Err(Error::RateLimited);
            }
        }
        let res = req
            .header("apikey", &self.key)
            .header("Application-Name", APP_NAME)
            .header("Application-Version", env!("CARGO_PKG_VERSION"))
            .send()
            .map_err(|e| Error::Other(format!("Nexus: {e}")))?;
        let h = res.headers();
        let num = |k: &str| h.get(k)?.to_str().ok()?.parse::<u32>().ok();
        let text = |k: &str| {
            h.get(k)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        if let (Some(hr), Some(dr)) = (num("x-rl-hourly-remaining"), num("x-rl-daily-remaining")) {
            let rl = RateLimit {
                hourly_remaining: hr,
                hourly_limit: num("x-rl-hourly-limit").unwrap_or(0),
                hourly_reset: text("x-rl-hourly-reset"),
                daily_remaining: dr,
                daily_limit: num("x-rl-daily-limit").unwrap_or(0),
                daily_reset: text("x-rl-daily-reset"),
            };
            // Nexus blocks once both quotas are spent.
            if hr == 0 && dr == 0 {
                *self.blocked_until.lock().unwrap() = Some(Instant::now() + BACKOFF);
            }
            *self.rate.lock().unwrap() = Some(rl);
        }
        let status = res.status().as_u16();
        let body = res
            .text()
            .map_err(|e| Error::Other(format!("Nexus: {e}")))?;
        match status {
            200..=299 => serde_json::from_str(&body)
                .map_err(|e| Error::Other(format!("Nexus: bad response: {e}"))),
            401 => Err(Error::Unauthorized),
            429 => {
                *self.blocked_until.lock().unwrap() = Some(Instant::now() + BACKOFF);
                Err(Error::RateLimited)
            }
            _ => {
                let msg = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| {
                        ["message", "detail", "error"]
                            .iter()
                            .find_map(|k| v[k].as_str().map(String::from))
                    })
                    .unwrap_or_else(|| body.chars().take(200).collect());
                Err(Error::Http {
                    status,
                    message: msg,
                })
            }
        }
    }
}

fn not_found(what: &str) -> Error {
    Error::Http {
        status: 404,
        message: format!("{what} not found"),
    }
}

/// Follow `(old, new)` replacements from `id` to the newest; cycle-safe.
fn follow_chain(mut id: u64, updates: &[(u64, u64)]) -> u64 {
    let mut seen = std::collections::HashSet::from([id]);
    while let Some(&(_, new)) = updates.iter().find(|(old, _)| *old == id) {
        if new == 0 || !seen.insert(new) {
            break;
        }
        id = new;
    }
    id
}

#[cfg(test)]
pub(crate) mod fake {
    //! One-thread HTTP/1.1 server answering canned responses; records each request.
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    pub struct Req {
        pub method: String,
        pub path: String,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Req {
        pub fn header(&self, k: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        }
    }

    pub struct Resp {
        pub status: u16,
        pub headers: Vec<(&'static str, String)>,
        pub body: Vec<u8>,
        /// Dribble the body out in 4 KiB chunks, 10 ms apart.
        pub slow: bool,
    }

    pub fn json(status: u16, body: &str) -> Resp {
        Resp {
            status,
            headers: vec![("content-type", "application/json".into())],
            body: body.as_bytes().to_vec(),
            slow: false,
        }
    }

    /// Serves until the test ends. Returns the base URL and the request log.
    pub fn serve(
        handler: impl Fn(&Req) -> Resp + Send + 'static,
    ) -> (String, Arc<Mutex<Vec<Req>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                if r.read_line(&mut line).is_err() || line.is_empty() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut headers = Vec::new();
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':') {
                        headers.push((k.trim().to_string(), v.trim().to_string()));
                    }
                }
                let len = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                let req = Req {
                    method,
                    path,
                    headers,
                    body: String::from_utf8_lossy(&body).into(),
                };
                let resp = handler(&req);
                log2.lock().unwrap().push(req);
                let mut out = format!("HTTP/1.1 {} X\r\nconnection: close\r\n", resp.status);
                for (k, v) in &resp.headers {
                    out += &format!("{k}: {v}\r\n");
                }
                out += &format!("content-length: {}\r\n\r\n", resp.body.len());
                let _ = s.write_all(out.as_bytes());
                if resp.slow {
                    std::thread::spawn(move || {
                        for c in resp.body.chunks(4096) {
                            if s.write_all(c).is_err() {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    });
                } else {
                    let _ = s.write_all(&resp.body);
                }
            }
        });
        (base, log)
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{json, serve, Resp};
    use super::*;

    const GQL_GAME: &str = r#"{"data":{"game":{"id":1704}}}"#;
    const GQL_MOD: &str = r#"{"data":{"legacyModsByDomain":{"nodes":[{"modId":12604,"gameId":1704,"name":"SkyUI","version":"6.11","author":"SkyUI Team","summary":"UI","modCategory":{"categoryId":42},"viewerEndorsed":true}]}}}"#;
    const GQL_FILES: &str = r#"{"data":{"modFiles":[
        {"fileId":10,"name":"SkyUI","version":"6.0","category":"ARCHIVED","date":100,"sizeInBytes":"5","uri":"a.zip","primary":0,"groupId":7},
        {"fileId":12,"name":"SkyUI","version":"6.2","category":"MAIN","date":300,"sizeInBytes":"6","uri":"c.zip","primary":1,"groupId":7},
        {"fileId":11,"name":"SkyUI","version":"6.1","category":"OLD_VERSION","date":200,"sizeInBytes":"5","uri":"b.zip","primary":0,"groupId":7},
        {"fileId":20,"name":"Patch","version":"1.0","category":"OPTIONAL","date":400,"sizeInBytes":null,"uri":"p.7z","primary":0,"groupId":8}]}}"#;

    fn gql(req: &super::fake::Req) -> Resp {
        let b = &req.body;
        if b.contains("game(domainName") {
            json(200, GQL_GAME)
        } else if b.contains("legacyModsByDomain") {
            json(200, GQL_MOD)
        } else if b.contains("modFiles") {
            json(200, GQL_FILES)
        } else {
            json(200, r#"{"errors":[{"message":"unexpected"}],"data":null}"#)
        }
    }

    #[test]
    fn graphql_mod_info_files_and_update_chain() {
        let (base, log) = serve(gql);
        let c = Client::with_base("KEY ", Api::Graphql, &base);
        let m = c.mod_info("skyrimspecialedition", 12604).unwrap();
        assert_eq!(
            (m.name.as_str(), m.category_id, m.endorsed, m.uid()),
            ("SkyUI", Some(42), Some(true), 7318624284988)
        );
        let files = c.files("skyrimspecialedition", 12604).unwrap();
        assert_eq!(files.len(), 4);
        assert_eq!(files[3].size, 0);
        // Update chain = same group, newest upload; a file alone in its group is its own latest.
        assert_eq!(
            c.latest("skyrimspecialedition", 12604, 10).unwrap().file_id,
            12
        );
        assert_eq!(
            c.latest("skyrimspecialedition", 12604, 20).unwrap().file_id,
            20
        );
        assert_eq!(
            c.file("skyrimspecialedition", 12604, 11).unwrap().version,
            "6.1"
        );
        let log = log.lock().unwrap();
        assert!(log
            .iter()
            .all(|r| r.path == "/v2/graphql" && r.header("apikey") == Some("KEY")));
        // mod_info learnt the game id; files() didn't look it up again.
        assert_eq!(
            log.iter()
                .filter(|r| r.body.contains("game(domainName"))
                .count(),
            0
        );
    }

    const V1_FILES: &str = r#"{"files":[
        {"file_id":10,"name":"SkyUI","version":"6.0","category_name":"OLD_VERSION","file_name":"a.zip","size_in_bytes":5,"uploaded_timestamp":100,"is_primary":false},
        {"file_id":11,"name":"SkyUI","version":"6.1","category_name":"MAIN","file_name":"b.zip","size_in_bytes":5,"uploaded_timestamp":200,"is_primary":true},
        {"file_id":12,"name":"Patch","version":"1.0","category_name":"OPTIONAL","file_name":"p.zip","size_in_bytes":5,"uploaded_timestamp":250,"is_primary":false}],
        "file_updates":[{"old_file_id":10,"new_file_id":11},{"old_file_id":11,"new_file_id":13},{"old_file_id":13,"new_file_id":11},{"old_file_id":12,"new_file_id":14}]}"#;

    fn v1(req: &super::fake::Req) -> Resp {
        match req.path.split('?').next().unwrap() {
            "/v1/games/skyrimspecialedition/mods/12604.json" => json(
                200,
                r#"{"mod_id":12604,"game_id":1704,"name":"SkyUI","version":"6.11","author":"SkyUI Team","summary":"UI","category_id":42,"endorsement":{"endorse_status":"Abstained"}}"#,
            ),
            "/v1/games/skyrimspecialedition/mods/12604/files.json" => json(200, V1_FILES),
            "/v1/games/skyrimspecialedition/mods/12604/files/14.json" => json(
                200,
                r#"{"file_id":14,"name":"Old","version":"0.1","category_name":"ARCHIVED","file_name":"o.zip","size_in_bytes":7,"uploaded_timestamp":1,"is_primary":false}"#,
            ),
            "/v1/games/skyrimspecialedition/mods/12604/files/13.json" => json(
                200,
                r#"{"file_id":13,"name":"SkyUI","version":"6.3","category_name":"MAIN","file_name":"d.zip","size_in_bytes":7,"uploaded_timestamp":300,"is_primary":true}"#,
            ),
            "/v1/games/skyrimspecialedition/mods/12604/files/11/download_link.json" => json(
                200,
                r#"[{"name":"Nexus CDN","short_name":"Nexus CDN","URI":"https://cdn/x.zip"}]"#,
            ),
            "/v1/users/validate.json" => json(
                200,
                r#"{"user_id":1,"key":"K","name":"me","is_premium":false,"is_supporter":true,"email":"x@y"}"#,
            ),
            "/v1/games/skyrimspecialedition.json" => json(
                200,
                r#"{"id":1704,"categories":[{"category_id":20,"name":"Skyrim Special Edition","parent_category":false},{"category_id":42,"name":"User Interface","parent_category":20}]}"#,
            ),
            "/v1/games/skyrimspecialedition/mods/updated.json" => {
                json(200, r#"[{"mod_id":5,"latest_file_update":1},{"mod_id":12604}]"#)
            }
            "/v1/games/skyrimspecialedition/mods/md5_search/3dc9d3b04e9b3edd1bafdbd0616c91bc.json" => json(
                200,
                r#"[{"mod":{"mod_id":12604},"file_details":{"file_id":735025,"name":"Patch","version":"1.1","category_name":"OPTIONAL","file_name":"p.7z","size_in_bytes":3439,"uploaded_timestamp":5,"is_primary":false}}]"#,
            ),
            "/v1/games/skyrimspecialedition/mods/12604/endorse.json" => json(200, r#"{"status":"Endorsed"}"#),
            _ => json(404, r#"{"message":"No Mod Found"}"#),
        }
    }

    #[test]
    fn rest_mode_parses_v1_and_follows_file_updates() {
        let (base, log) = serve(v1);
        let c = Client::with_base("KEY", Api::Rest, &base);
        let m = c.mod_info("skyrimspecialedition", 12604).unwrap();
        assert_eq!((m.version.as_str(), m.endorsed), ("6.11", Some(false)));
        assert_eq!(c.files("skyrimspecialedition", 12604).unwrap().len(), 3);
        // 10 -> 11 -> 13 (-> 11 loop stops); 13 isn't in the list, so it's fetched by id.
        let l = c.latest("skyrimspecialedition", 12604, 10).unwrap();
        assert_eq!((l.file_id, l.version.as_str()), (13, "6.3"));
        // A "replacement" uploaded earlier than the file isn't an update.
        assert_eq!(
            c.latest("skyrimspecialedition", 12604, 12).unwrap().file_id,
            12
        );
        let hits = c
            .by_md5("skyrimspecialedition", "3dc9d3b04e9b3edd1bafdbd0616c91bc")
            .unwrap();
        assert_eq!((hits[0].mod_id, hits[0].file.file_id), (12604, 735025));
        c.endorse("skyrimspecialedition", 12604, "6.11", true)
            .unwrap();
        let e = c.mod_info("skyrimspecialedition", 1).unwrap_err();
        assert!(
            matches!(&e, Error::Http { status: 404, message } if message == "No Mod Found"),
            "{e}"
        );
        let log = log.lock().unwrap();
        let endorse = log
            .iter()
            .find(|r| r.path.ends_with("endorse.json"))
            .unwrap();
        assert_eq!(
            (endorse.method.as_str(), endorse.body.as_str()),
            ("POST", "version=6.11")
        );
    }

    /// GraphQL mode sends what GraphQL can't answer to v1.
    #[test]
    fn graphql_mode_falls_back_to_v1() {
        let (base, log) = serve(|r| {
            if r.path == "/v2/graphql" {
                gql(r)
            } else {
                v1(r)
            }
        });
        let c = Client::with_base("KEY", Api::Graphql, &base);
        let u = c.validate().unwrap();
        assert_eq!(
            (u.name.as_str(), u.premium, u.supporter),
            ("me", false, true)
        );
        let key = NxmKey {
            key: "abc".into(),
            expires: 99,
        };
        let links = c
            .download_links("skyrimspecialedition", 12604, 11, Some(&key))
            .unwrap();
        assert_eq!(links[0].url, "https://cdn/x.zip");
        assert_eq!(
            c.updated("skyrimspecialedition", Period::Week).unwrap(),
            [5, 12604]
        );
        let cats = c.categories("skyrimspecialedition").unwrap();
        assert_eq!((cats[0].parent, cats[1].parent), (None, Some(20)));
        let log = log.lock().unwrap();
        assert!(log.iter().all(|r| r.path.starts_with("/v1/")));
        assert!(log
            .iter()
            .any(|r| r.path.ends_with("download_link.json?key=abc&expires=99")));
        assert!(log
            .iter()
            .any(|r| r.path.ends_with("updated.json?period=1w")));
    }

    #[test]
    fn errors_and_rate_limits() {
        let (base, log) = serve(|r| {
            let rl = |h: &str, d: &str| {
                vec![
                    ("x-rl-hourly-remaining", h.to_string()),
                    ("x-rl-hourly-limit", "100".into()),
                    ("x-rl-hourly-reset", "2026-10-10 20:00:00 +0000".into()),
                    ("x-rl-daily-remaining", d.to_string()),
                    ("x-rl-daily-limit", "2000".into()),
                ]
            };
            match r.path.as_str() {
                "/v1/users/validate.json" if r.header("apikey") == Some("bad") => {
                    json(401, r#"{"message":"Please provide a valid API Key"}"#)
                }
                "/v2/graphql" => json(
                    200,
                    r#"{"errors":[{"message":"Game not found","extensions":{"code":"GAME_NOT_FOUND"}}],"data":{"game":null}}"#,
                ),
                _ => Resp {
                    headers: rl("0", "0"),
                    ..json(429, "{}")
                },
            }
        });
        let bad = Client::with_base("bad", Api::Graphql, &base);
        assert!(matches!(bad.validate(), Err(Error::Unauthorized)));
        let e = bad.mod_info("nope", 1).unwrap_err();
        assert!(matches!(e, Error::Http { status: 404, .. }), "{e}");

        let c = Client::with_base("k", Api::Rest, &base);
        assert!(matches!(c.mod_info("x", 1), Err(Error::RateLimited)));
        let rl = c.rate_limit().unwrap();
        assert_eq!((rl.hourly_remaining, rl.daily_limit), (0, 2000));
        // Blocked: the next call fails locally without a request.
        let before = log.lock().unwrap().len();
        assert!(matches!(c.files("x", 1), Err(Error::RateLimited)));
        assert_eq!(log.lock().unwrap().len(), before);
    }

    /// Against the real API when a key is available (`NEXUS_API_KEY` or `rust/NEXUS_API_KEY.env.user`).
    /// Read-only: never endorses (that would change the account).
    #[test]
    fn live_api() {
        let Some(key) = live_key() else {
            return eprintln!("live_api skipped: no NEXUS_API_KEY");
        };
        let d = "skyrimspecialedition";
        // SkyUI's "Simplified Chinese Patch" 1.1 (3439 bytes).
        let md5 = "3dc9d3b04e9b3edd1bafdbd0616c91bc";
        for api in [Api::Graphql, Api::Rest] {
            let c = Client::new(&key, api);
            let m = c.mod_info(d, 12604).unwrap();
            assert_eq!(
                (m.name.as_str(), m.game_id, m.category_id),
                ("SkyUI", 1704, Some(42)),
                "{api:?}"
            );
            let files = c.files(d, 12604).unwrap();
            assert!(files.iter().any(|f| f.file_id == 735025), "{api:?}");
            let hit = &c.by_md5(d, md5).unwrap()[0];
            assert_eq!(
                (hit.mod_id, hit.file.file_id, hit.file.size),
                (12604, 735025, 3439),
                "{api:?}"
            );
            let latest = c.latest(d, 12604, 735025).unwrap();
            assert!(latest.uploaded >= hit.file.uploaded, "{api:?}");
            // Only v1 sends X-RL-* headers.
            assert_eq!(c.rate_limit().is_some(), api == Api::Rest);
        }
        let c = Client::new(&key, Api::Graphql);
        assert!(c.validate().unwrap().user_id > 0);
        assert!(c.categories(d).unwrap().iter().any(|c| c.id == 42));
        if c.validate().unwrap().premium {
            assert!(!c.download_links(d, 12604, 735025, None).unwrap().is_empty());
        }
        assert!(matches!(
            Client::new("nope", Api::Rest).validate(),
            Err(Error::Unauthorized)
        ));
    }

    pub(crate) fn live_key() -> Option<String> {
        std::env::var("NEXUS_API_KEY")
            .ok()
            .or_else(|| {
                std::fs::read_to_string(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../NEXUS_API_KEY.env.user"
                ))
                .ok()
            })
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
    }
}
