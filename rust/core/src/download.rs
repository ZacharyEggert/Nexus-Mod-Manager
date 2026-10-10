//! Download queue (intent of C# `DownloadManagement` + `DownloadMonitoring`): concurrent limit,
//! pause/resume via HTTP Range, retry with backoff over the CDN links, md5 check, and a
//! `downloads.json` that survives restarts.

use crate::nexus::NxmKey;
use crate::{Error, IoContext, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Download {
    pub id: String,
    /// Game mode it's for (where Install puts it).
    pub mode: String,
    pub domain: String,
    pub mod_id: u64,
    pub file_id: u64,
    pub mod_name: String,
    /// File display name.
    pub name: String,
    pub version: String,
    /// Where the finished archive lands (`<file>.part` while downloading).
    pub path: PathBuf,
    /// Bytes; 0 = unknown.
    pub size: u64,
    /// Non-premium download key from the nxm link.
    pub key: Option<NxmKey>,
    pub state: State,
    /// Bytes on disk so far (live; the `.part` length is what resume trusts).
    #[serde(default)]
    pub done: u64,
    pub error: Option<String>,
    /// md5 checked against Nexus: `Some(true)` matched, `None` couldn't check.
    pub verified: Option<bool>,
}

type LinksFn = dyn Fn(&Download) -> Result<Vec<String>> + Send + Sync;
type VerifyFn = dyn Fn(&Download, &str) -> Result<bool> + Send + Sync;

/// What the queue needs from the outside (the Nexus client in the app; fakes in tests).
pub struct Hooks {
    /// Fresh CDN links for a download.
    pub links: Box<LinksFn>,
    /// Does this md5 belong to the download's Nexus file?
    pub verify: Box<VerifyFn>,
    /// A download changed (state or progress, throttled).
    pub changed: Box<dyn Fn(&Download) + Send + Sync>,
}

/// Attempts per download before it fails (each failure rotates to the next CDN link).
const MAX_ATTEMPTS: u32 = 5;
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Downloads(Arc<Inner>);

struct Inner {
    file: PathBuf,
    list: Mutex<Vec<Download>>,
    /// Stop flags of running downloads.
    running: Mutex<HashMap<String, Arc<AtomicBool>>>,
    max: AtomicUsize,
    hooks: Hooks,
    /// First retry wait; doubles per attempt (tests shrink it).
    retry_delay: Duration,
    http: reqwest::blocking::Client,
}

enum Fetch {
    Complete,
    Stopped,
}

enum Fail {
    /// Link expired or gone: fetch new links.
    Expired(String),
    /// Network trouble or server error: back off, try the next link.
    Transient(String),
    Fatal(Error),
}

impl Downloads {
    /// Load `downloads.json` from `dir`; interrupted downloads resume.
    pub fn open(dir: &Path, max: usize, hooks: Hooks) -> Result<Self> {
        Self::open_with(dir, max, hooks, Duration::from_secs(2))
    }

    pub(crate) fn open_with(
        dir: &Path,
        max: usize,
        hooks: Hooks,
        retry_delay: Duration,
    ) -> Result<Self> {
        fs::create_dir_all(dir).at(dir)?;
        let file = dir.join("downloads.json");
        let mut list: Vec<Download> = match fs::read(&file) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::error!(
                    "{}: {e}; starting with an empty download list",
                    file.display()
                );
                vec![]
            }),
            Err(_) => vec![],
        };
        for d in &mut list {
            if d.state == State::Running {
                d.state = State::Queued;
            }
        }
        let me = Self(Arc::new(Inner {
            file,
            list: Mutex::new(list),
            running: Mutex::default(),
            max: AtomicUsize::new(max.max(1)),
            hooks,
            retry_delay,
            http: reqwest::blocking::Client::builder()
                .user_agent(concat!("NexusModManagerRs/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(30))
                // ponytail: no whole-request timeout (big files); a stalled CDN is caught by TCP keepalive.
                .timeout(None)
                .tcp_keepalive(Duration::from_secs(30))
                .build()
                .expect("http client"),
        }));
        me.pump();
        Ok(me)
    }

    pub fn list(&self) -> Vec<Download> {
        self.0.list.lock().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<Download> {
        self.0
            .list
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.id == id)
            .cloned()
    }

    pub fn set_max(&self, n: usize) {
        self.0.max.store(n.max(1), Ordering::Relaxed);
        self.pump();
    }

    /// Queue `d` (its `id` is derived from game + file). The same file already queued, running
    /// or done is not added twice; a failed or paused one is restarted.
    pub fn add(&self, mut d: Download) -> String {
        d.id = format!("{}-{}", d.mode, d.file_id);
        let id = d.id.clone();
        {
            let mut list = self.0.list.lock().unwrap();
            match list.iter_mut().find(|x| x.id == id) {
                Some(x) if matches!(x.state, State::Failed | State::Paused) => {
                    x.state = State::Queued;
                    x.error = None;
                    x.key = d.key.or(x.key.take());
                }
                Some(_) => return id,
                None => {
                    d.state = State::Queued;
                    d.error = None;
                    d.verified = None;
                    d.done = part(&d.path).metadata().map(|m| m.len()).unwrap_or(0);
                    list.push(d);
                }
            }
        }
        self.saved_change(&id);
        self.pump();
        id
    }

    pub fn pause(&self, id: &str) {
        if let Some(stop) = self.0.running.lock().unwrap().get(id) {
            stop.store(true, Ordering::Relaxed); // the worker marks it Paused
            return;
        }
        self.set_state(id, |d| d.state == State::Queued, State::Paused);
    }

    pub fn resume(&self, id: &str) {
        self.set_state(
            id,
            |d| matches!(d.state, State::Paused | State::Failed),
            State::Queued,
        );
        self.pump();
    }

    /// Drop from the list; partial data is deleted, a finished archive only with `delete_file`.
    pub fn remove(&self, id: &str, delete_file: bool) {
        let d = {
            let mut list = self.0.list.lock().unwrap();
            let Some(i) = list.iter().position(|d| d.id == id) else {
                return;
            };
            list.remove(i)
        };
        // A running worker sees its entry gone when it stops, and deletes the .part itself.
        if let Some(stop) = self.0.running.lock().unwrap().get(id) {
            stop.store(true, Ordering::Relaxed);
        } else {
            let _ = fs::remove_file(part(&d.path));
        }
        if delete_file && d.state == State::Done {
            let _ = fs::remove_file(&d.path);
        }
        self.save();
    }

    fn set_state(&self, id: &str, when: impl Fn(&Download) -> bool, to: State) {
        let hit = {
            let mut list = self.0.list.lock().unwrap();
            match list.iter_mut().find(|d| d.id == id && when(d)) {
                Some(d) => {
                    d.state = to;
                    if to == State::Queued {
                        d.error = None;
                    }
                    true
                }
                None => false,
            }
        };
        if hit {
            self.saved_change(id);
        }
    }

    fn save(&self) {
        let bytes = serde_json::to_vec_pretty(&*self.0.list.lock().unwrap()).unwrap();
        if let Err(e) = crate::write_durable(&self.0.file, &bytes) {
            tracing::error!("saving download list: {e}");
        }
    }

    fn saved_change(&self, id: &str) {
        self.save();
        if let Some(d) = self.get(id) {
            (self.0.hooks.changed)(&d);
        }
    }

    /// Start queued downloads up to the limit.
    fn pump(&self) {
        let mut running = self.0.running.lock().unwrap();
        let start: Vec<String> = {
            let mut list = self.0.list.lock().unwrap();
            let free = self
                .0
                .max
                .load(Ordering::Relaxed)
                .saturating_sub(running.len());
            list.iter_mut()
                .filter(|d| d.state == State::Queued)
                .take(free)
                .map(|d| {
                    d.state = State::Running;
                    d.id.clone()
                })
                .collect()
        };
        for id in start {
            let stop = Arc::new(AtomicBool::new(false));
            running.insert(id.clone(), stop.clone());
            let me = self.clone();
            std::thread::spawn(move || me.work(&id, &stop));
        }
        drop(running);
        self.save();
    }

    fn work(&self, id: &str, stop: &AtomicBool) {
        if let Some(d) = self.get(id) {
            (self.0.hooks.changed)(&d);
            tracing::info!(download = %id, file = %d.name, "download started");
            let path = d.path.clone();
            let res = self.run(d, stop);
            let mut list = self.0.list.lock().unwrap();
            match list.iter_mut().find(|d| d.id == id) {
                Some(d) => match res {
                    Ok((Fetch::Complete, verified)) => {
                        tracing::info!(download = %id, ?verified, "download finished");
                        d.state = State::Done;
                        d.verified = verified;
                        d.done = d.size.max(d.done);
                    }
                    Ok((Fetch::Stopped, _)) => d.state = State::Paused,
                    Err(e) => {
                        tracing::error!(download = %id, "download failed: {e}");
                        d.state = State::Failed;
                        d.error = Some(e.to_string());
                    }
                },
                // Removed while running.
                None => {
                    let _ = fs::remove_file(part(&path));
                }
            }
        }
        // Persist the outcome before the slot frees, so a quit right now can't lose it.
        self.save();
        self.0.running.lock().unwrap().remove(id);
        if let Some(d) = self.get(id) {
            (self.0.hooks.changed)(&d);
        }
        self.pump();
    }

    /// Download until complete, stopped, or out of attempts; then check the md5.
    fn run(&self, d: Download, stop: &AtomicBool) -> Result<(Fetch, Option<bool>)> {
        let part = part(&d.path);
        if let Some(dir) = d.path.parent() {
            fs::create_dir_all(dir).at(dir)?;
        }
        let mut links: Vec<String> = vec![];
        let (mut attempt, mut next, mut rechecked) = (0, 0, false);
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok((Fetch::Stopped, None));
            }
            if links.is_empty() {
                links = (self.0.hooks.links)(&d)?;
                if links.is_empty() {
                    return Err("Nexus returned no download links".into());
                }
            }
            let url = links[next % links.len()].clone();
            match self.fetch(&d, &url, &part, stop) {
                Ok(Fetch::Stopped) => return Ok((Fetch::Stopped, None)),
                Ok(Fetch::Complete) => {
                    let md5 = md5_file(&part)?;
                    let verified = match (self.0.hooks.verify)(&d, &md5) {
                        Ok(true) => Some(true),
                        // Corrupt (or not this file): start over once, then give up.
                        Ok(false) if !rechecked => {
                            tracing::warn!(download = %d.id, md5, "md5 doesn't match Nexus; downloading again");
                            rechecked = true;
                            fs::remove_file(&part).at(&part)?;
                            continue;
                        }
                        Ok(false) => {
                            let _ = fs::remove_file(&part);
                            return Err(
                                "downloaded file doesn't match Nexus (md5); try again later".into(),
                            );
                        }
                        Err(e) => {
                            tracing::warn!(download = %d.id, "md5 check skipped: {e}");
                            None
                        }
                    };
                    fs::rename(&part, &d.path).at(&d.path)?;
                    return Ok((Fetch::Complete, verified));
                }
                Err(fail) => {
                    attempt += 1;
                    let why = match fail {
                        Fail::Fatal(e) => return Err(e),
                        Fail::Expired(why) => {
                            links.clear();
                            why
                        }
                        Fail::Transient(why) => {
                            next += 1;
                            why
                        }
                    };
                    if attempt >= MAX_ATTEMPTS {
                        return Err(
                            format!("download failed after {attempt} attempts: {why}").into()
                        );
                    }
                    let wait = (self.0.retry_delay * 2u32.pow(attempt - 1)).min(MAX_BACKOFF);
                    tracing::warn!(download = %d.id, attempt, ?wait, "{why}; retrying");
                    self.progress(
                        &d.id,
                        None,
                        Some(format!("{why}; retrying ({attempt}/{MAX_ATTEMPTS})")),
                    );
                    let until = Instant::now() + wait;
                    while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
    }

    /// One GET of `url` into `part`, resuming from its current length.
    fn fetch(
        &self,
        d: &Download,
        url: &str,
        part: &Path,
        stop: &AtomicBool,
    ) -> Result<Fetch, Fail> {
        let have = part.metadata().map(|m| m.len()).unwrap_or(0);
        if d.size > 0 && have == d.size {
            return Ok(Fetch::Complete);
        }
        let mut req = self.0.http.get(url);
        if have > 0 {
            req = req.header("Range", format!("bytes={have}-"));
        }
        let mut res = req.send().map_err(|e| Fail::Transient(e.to_string()))?;
        let status = res.status().as_u16();
        let append = match status {
            206 => true,
            200 => false, // no range support: start over
            416 => {
                // Our .part is longer than the file: corrupt; start over.
                let _ = fs::remove_file(part);
                return Err(Fail::Transient("server rejected resume range".into()));
            }
            401 | 403 | 404 | 410 => return Err(Fail::Expired(format!("HTTP {status}"))),
            429 | 500..=599 => return Err(Fail::Transient(format!("HTTP {status}"))),
            _ => {
                return Err(Fail::Fatal(
                    format!("HTTP {status} from download server").into(),
                ))
            }
        };
        let expect = res
            .content_length()
            .map(|n| n + if append { have } else { 0 });
        let fatal = |e: std::io::Error| {
            Fail::Fatal(Error::Io {
                path: part.into(),
                source: e,
            })
        };
        let mut f = if append {
            OpenOptions::new().append(true).open(part).map_err(fatal)?
        } else {
            File::create(part).map_err(fatal)?
        };
        let mut done = if append { have } else { 0 };
        let mut buf = vec![0; 64 * 1024];
        let mut last = Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) {
                f.sync_all().map_err(fatal)?;
                return Ok(Fetch::Stopped);
            }
            let n = res
                .read(&mut buf)
                .map_err(|e| Fail::Transient(e.to_string()))?;
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n]).map_err(fatal)?;
            done += n as u64;
            if last.elapsed() >= Duration::from_millis(250) {
                last = Instant::now();
                self.progress(&d.id, Some(done), None);
            }
        }
        f.sync_all().map_err(fatal)?;
        self.progress(&d.id, Some(done), None);
        match expect {
            Some(n) if n != done => Err(Fail::Transient(format!(
                "connection dropped at {done}/{n} bytes"
            ))),
            _ => Ok(Fetch::Complete),
        }
    }

    /// Live progress/notes (not persisted; the .part length is the truth on restart).
    fn progress(&self, id: &str, done: Option<u64>, note: Option<String>) {
        let d = {
            let mut list = self.0.list.lock().unwrap();
            let Some(d) = list.iter_mut().find(|d| d.id == id) else {
                return;
            };
            if let Some(n) = done {
                d.done = n;
                d.error = None;
            }
            if note.is_some() {
                d.error = note;
            }
            d.clone()
        };
        (self.0.hooks.changed)(&d);
    }
}

fn part(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".part");
    p.into()
}

pub fn md5_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).at(path)?;
    let mut ctx = md5::Context::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = f.read(&mut buf).at(path)?;
        if n == 0 {
            break;
        }
        ctx.consume(&buf[..n]);
    }
    Ok(format!("{:x}", ctx.finalize()))
}

/// A file name from Nexus that's safe to join onto a folder.
pub fn safe_file_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || r#"/\:*?"<>|"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    match s.trim().trim_matches('.') {
        "" => "download".into(),
        t => t.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::fake::{serve, Req, Resp};
    use std::sync::atomic::AtomicU32;

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 % 251) as u8).collect()
    }

    /// File server with Range support; `fail` decides per request whether to answer with a status instead.
    fn file_server(
        body: Vec<u8>,
        slow: bool,
        fail: impl Fn(&Req, u32) -> Option<u16> + Send + 'static,
    ) -> (String, Arc<Mutex<Vec<Req>>>) {
        let n = AtomicU32::new(0);
        serve(move |r| {
            let i = n.fetch_add(1, Ordering::Relaxed);
            if let Some(status) = fail(r, i) {
                return Resp {
                    status,
                    headers: vec![],
                    body: vec![],
                    slow: false,
                };
            }
            let from = r.header("range").and_then(|v| {
                v.strip_prefix("bytes=")?
                    .strip_suffix('-')?
                    .parse::<usize>()
                    .ok()
            });
            match from {
                Some(f) if f >= body.len() => Resp {
                    status: 416,
                    headers: vec![],
                    body: vec![],
                    slow: false,
                },
                Some(f) => Resp {
                    status: 206,
                    headers: vec![(
                        "content-range",
                        format!("bytes {f}-{}/{}", body.len() - 1, body.len()),
                    )],
                    body: body[f..].to_vec(),
                    slow,
                },
                None => Resp {
                    status: 200,
                    headers: vec![],
                    body: body.clone(),
                    slow,
                },
            }
        })
    }

    fn item(dir: &Path, size: usize) -> Download {
        Download {
            id: String::new(),
            mode: "SkyrimSE".into(),
            domain: "skyrimspecialedition".into(),
            mod_id: 1,
            file_id: 2,
            mod_name: "M".into(),
            name: "F".into(),
            version: "1".into(),
            path: dir.join("SkyrimSE/f.7z"),
            size: size as u64,
            key: None,
            state: State::Queued,
            done: 0,
            error: None,
            verified: None,
        }
    }

    /// Links = `urls` (in order, rotating per call), md5 must equal `good`.
    fn hooks(urls: Vec<String>, good: String, calls: Arc<AtomicU32>) -> Hooks {
        Hooks {
            links: Box::new(move |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(urls.clone())
            }),
            verify: Box::new(move |_, md5| Ok(md5 == good)),
            changed: Box::new(|_| {}),
        }
    }

    fn wait(dl: &Downloads, id: &str, want: State) -> Download {
        let t = Instant::now();
        loop {
            let d = dl.get(id).unwrap();
            if d.state == want && dl.0.running.lock().unwrap().is_empty() {
                return d;
            }
            assert!(
                t.elapsed() < Duration::from_secs(10),
                "stuck in {:?}: {:?}",
                d.state,
                d.error
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn md5_of(b: &[u8]) -> String {
        format!("{:x}", md5::compute(b))
    }

    #[test]
    fn downloads_verifies_and_survives_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let body = data(300_000);
        let (base, log) = file_server(body.clone(), false, |_, _| None);
        let calls = Arc::new(AtomicU32::new(0));
        let h = || hooks(vec![format!("{base}/f.7z")], md5_of(&body), calls.clone());
        // A crash left half a .part and the entry "running".
        let mut d = item(tmp.path(), body.len());
        d.id = "SkyrimSE-2".into();
        d.state = State::Running;
        fs::create_dir_all(tmp.path().join("SkyrimSE")).unwrap();
        fs::write(part(&d.path), &body[..100_000]).unwrap();
        fs::write(
            tmp.path().join("downloads.json"),
            serde_json::to_vec(&[&d]).unwrap(),
        )
        .unwrap();

        let dl = Downloads::open_with(tmp.path(), 2, h(), Duration::from_millis(1)).unwrap();
        let done = wait(&dl, "SkyrimSE-2", State::Done);
        assert_eq!(fs::read(&done.path).unwrap(), body);
        assert_eq!(done.verified, Some(true));
        assert!(!part(&done.path).exists());
        assert_eq!(
            log.lock().unwrap()[0].header("range"),
            Some("bytes=100000-")
        );
        // Same file again: not re-downloaded.
        assert_eq!(dl.add(item(tmp.path(), body.len())), "SkyrimSE-2");
        assert_eq!(dl.list().len(), 1);
        // The list is on disk.
        drop(dl);
        let again = Downloads::open_with(tmp.path(), 2, h(), Duration::from_millis(1)).unwrap();
        assert_eq!(again.get("SkyrimSE-2").unwrap().state, State::Done);
    }

    #[test]
    fn pause_resume_and_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let body = data(400_000); // ~1 s at the slow server's pace
        let (base, log) = file_server(body.clone(), true, |_, _| None);
        let calls = Arc::new(AtomicU32::new(0));
        let dl = Downloads::open_with(
            tmp.path(),
            1,
            hooks(vec![format!("{base}/f")], md5_of(&body), calls),
            Duration::from_millis(1),
        )
        .unwrap();
        let id = dl.add(item(tmp.path(), body.len()));
        std::thread::sleep(Duration::from_millis(300));
        dl.pause(&id);
        let p = wait(&dl, &id, State::Paused);
        let have = part(&p.path).metadata().unwrap().len();
        assert!(have > 0 && have < body.len() as u64, "{have}");
        dl.resume(&id);
        let d = wait(&dl, &id, State::Done);
        assert_eq!(fs::read(&d.path).unwrap(), body);
        assert_eq!(
            log.lock().unwrap().last().unwrap().header("range"),
            Some(&*format!("bytes={have}-"))
        );

        // Removing mid-download deletes the partial file.
        let mut other = item(tmp.path(), body.len());
        other.file_id = 3;
        other.path = tmp.path().join("SkyrimSE/g.7z");
        let id2 = dl.add(other.clone());
        std::thread::sleep(Duration::from_millis(200));
        dl.remove(&id2, false);
        let t = Instant::now();
        while !dl.0.running.lock().unwrap().is_empty() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(dl.get(&id2).is_none() && !part(&other.path).exists());
        dl.remove(&id, true);
        assert!(!d.path.exists());
    }

    #[test]
    fn retries_rotate_links_and_refresh_expired_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let body = data(50_000);
        // Request 0: CDN A 503; request 1: CDN B 403 (expired) -> links fetched again; then OK.
        let (base, log) = file_server(body.clone(), false, |_, i| match i {
            0 => Some(503),
            1 => Some(403),
            _ => None,
        });
        let calls = Arc::new(AtomicU32::new(0));
        let urls = vec![format!("{base}/a"), format!("{base}/b")];
        let dl = Downloads::open_with(
            tmp.path(),
            1,
            hooks(urls, md5_of(&body), calls.clone()),
            Duration::from_millis(1),
        )
        .unwrap();
        let id = dl.add(item(tmp.path(), body.len()));
        let d = wait(&dl, &id, State::Done);
        assert_eq!(fs::read(&d.path).unwrap(), body);
        let paths: Vec<String> = log.lock().unwrap().iter().map(|r| r.path.clone()).collect();
        assert_eq!(paths, ["/a", "/b", "/b"]);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn bad_md5_retries_once_then_fails_and_server_errors_give_up() {
        let tmp = tempfile::tempdir().unwrap();
        let body = data(10_000);
        let (base, log) = file_server(body.clone(), false, |_, _| None);
        let calls = Arc::new(AtomicU32::new(0));
        let dl = Downloads::open_with(
            tmp.path(),
            1,
            hooks(vec![format!("{base}/f")], "nope".into(), calls),
            Duration::from_millis(1),
        )
        .unwrap();
        let id = dl.add(item(tmp.path(), body.len()));
        let d = wait(&dl, &id, State::Failed);
        assert!(d.error.unwrap().contains("md5"));
        assert_eq!(log.lock().unwrap().len(), 2);
        assert!(!part(&d.path).exists() && !d.path.exists());

        let (base, log) = file_server(body.clone(), false, |_, _| Some(500));
        let calls = Arc::new(AtomicU32::new(0));
        let dl = Downloads::open_with(
            &tmp.path().join("b"),
            1,
            hooks(vec![format!("{base}/f")], md5_of(&body), calls),
            Duration::from_millis(1),
        )
        .unwrap();
        let id = dl.add(item(tmp.path(), body.len()));
        let d = wait(&dl, &id, State::Failed);
        assert!(d.error.unwrap().contains("after 5 attempts"), "");
        assert_eq!(log.lock().unwrap().len(), MAX_ATTEMPTS as usize);
        // Resume = try again.
        dl.resume(&id);
        assert_ne!(dl.get(&id).unwrap().state, State::Failed);
    }

    #[test]
    fn server_without_range_restarts_and_names_are_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let body = data(20_000);
        // Ignores Range: always the whole file with 200.
        let b2 = body.clone();
        let (base, _) = serve(move |_| Resp {
            status: 200,
            headers: vec![],
            body: b2.clone(),
            slow: false,
        });
        let calls = Arc::new(AtomicU32::new(0));
        let mut d = item(tmp.path(), body.len());
        fs::create_dir_all(d.path.parent().unwrap()).unwrap();
        fs::write(part(&d.path), b"stale garbage").unwrap();
        d.size = 0; // unknown size: can't short-cut on length
        let dl = Downloads::open_with(
            tmp.path(),
            1,
            hooks(vec![format!("{base}/f")], md5_of(&body), calls),
            Duration::from_millis(1),
        )
        .unwrap();
        let id = dl.add(d);
        let d = wait(&dl, &id, State::Done);
        assert_eq!(fs::read(&d.path).unwrap(), body);
        assert_eq!(safe_file_name("../a/b:c?.7z"), "_a_b_c_.7z");
        assert_eq!(safe_file_name(".."), "download");
    }
}
