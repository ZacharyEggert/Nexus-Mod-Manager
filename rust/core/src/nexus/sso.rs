//! Nexus single sign-on (port of C# `SsoManager`): the user approves this app in the browser and
//! the SSO websocket hands back their API key.

use crate::{Error, Result, Task};
use serde_json::{json, Value};
use std::io::ErrorKind;
use std::net::TcpStream;
use std::time::Duration;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

pub const URL: &str = "wss://sso.nexusmods.com";

/// This app's Nexus application slug, baked in at build time (`NEXUS_SSO_APP=...`).
/// Without one, SSO is off and only manual key entry works.
pub const APP: Option<&str> = option_env!("NEXUS_SSO_APP");

/// Connect attempts in a row that may fail before giving up.
const MAX_RETRIES: u32 = 5;

/// The page where the user approves the app.
pub fn approval_page(id: &str, app: &str) -> String {
    format!("https://www.nexusmods.com/sso?id={id}&application={app}")
}

/// Run an SSO session against `url`; `open` shows the approval page (once). Reconnects with the
/// session's connection token when the socket drops; returns the API key, or `Cancelled`.
pub fn login(url: &str, app: &str, task: &Task, open: &dyn Fn(&str)) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let mut token: Option<String> = None;
    let mut opened = false;
    let mut failures = 0;
    loop {
        task.step("Waiting for approval in the browser", 0, 0)?;
        let mut ws = match tungstenite::connect(url) {
            Ok((ws, _)) => ws,
            Err(e) => {
                failures += 1;
                if failures >= MAX_RETRIES {
                    return Err(Error::Other(format!("Nexus SSO: can't connect: {e}")));
                }
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        failures = 0;
        poll_with_timeout(&ws);
        let hello = json!({ "id": id, "token": token, "protocol": 2 });
        if ws.send(Message::text(hello.to_string())).is_err() {
            continue;
        }
        if !opened {
            open(&approval_page(&id, app));
            opened = true;
        }
        loop {
            if task.cancelled() {
                let _ = ws.close(None);
                return Err(Error::Cancelled);
            }
            match ws.read() {
                Ok(Message::Text(t)) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or_default();
                    if v["success"] != true {
                        let why = v["error"].as_str().unwrap_or(&t);
                        return Err(Error::Other(format!("Nexus SSO: {why}")));
                    }
                    if let Some(key) = v["data"]["api_key"].as_str() {
                        let _ = ws.close(None);
                        return Ok(key.into());
                    }
                    if let Some(t) = v["data"]["connection_token"].as_str() {
                        token = Some(t.into());
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                // Dropped: reconnect and resume with the token.
                Err(_) => break,
            }
        }
    }
}

/// Short read timeout so the loop can notice a cancel.
fn poll_with_timeout(ws: &WebSocket<MaybeTlsStream<TcpStream>>) {
    let sock = match ws.get_ref() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::Rustls(s) => &s.sock,
        _ => return,
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(250)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    fn hello(ws: &mut WebSocket<TcpStream>) -> Value {
        serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap()
    }

    #[test]
    fn sso_reconnects_with_token_and_returns_key() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", l.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut a = tungstenite::accept(l.accept().unwrap().0).unwrap();
            let h1 = hello(&mut a);
            assert_eq!(
                (h1["protocol"].as_i64(), h1["token"].is_null()),
                (Some(2), true)
            );
            let token = r#"{"success":true,"data":{"connection_token":"T"},"error":null}"#;
            a.send(Message::text(token)).unwrap();
            drop(a); // socket drops before approval
            let mut b = tungstenite::accept(l.accept().unwrap().0).unwrap();
            let h2 = hello(&mut b);
            assert_eq!(
                (h2["id"].clone(), h2["token"].as_str()),
                (h1["id"].clone(), Some("T"))
            );
            let key = r#"{"success":true,"data":{"api_key":"KEY"},"error":null}"#;
            b.send(Message::text(key)).unwrap();
            let _ = b.read(); // client's close
            h1["id"].as_str().unwrap().to_string()
        });
        let opened = Mutex::new(vec![]);
        let key = login(&url, "myapp", &Task::default(), &|u| {
            opened.lock().unwrap().push(u.to_string())
        })
        .unwrap();
        let id = server.join().unwrap();
        assert_eq!(key, "KEY");
        assert_eq!(*opened.lock().unwrap(), [approval_page(&id, "myapp")]);
    }

    #[test]
    fn sso_cancel_and_server_error() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", l.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut a = tungstenite::accept(l.accept().unwrap().0).unwrap();
            hello(&mut a);
            let mut b = tungstenite::accept(l.accept().unwrap().0).unwrap();
            hello(&mut b);
            let err = r#"{"success":false,"data":null,"error":"Invalid application"}"#;
            b.send(Message::text(err)).unwrap();
            let _ = a.read(); // hold the first socket open until the client closes it
        });
        let task = Task::default();
        let cancel = task.cancel.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            cancel.store(true, Ordering::Relaxed);
        });
        let opened = Arc::new(Mutex::new(0));
        let r = login(&url, "x", &task, &|_| *opened.lock().unwrap() += 1);
        t.join().unwrap();
        assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
        let e = login(&url, "x", &Task::default(), &|_| {}).unwrap_err();
        assert_eq!(e.to_string(), "Nexus SSO: Invalid application");
    }
}
