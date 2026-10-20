//! Sync between the desktop (server) and the phone (client) on the local network.
//!
//! Discovery: the phone broadcasts `teach-me? <device>` over UDP, the desktop
//! answers unicast `teach-me! <device> <http port>`. Then plain HTTP with the
//! pairing key as a bearer token:
//! - `GET /manifest`     phrases and clip list
//! - `GET /clips/<file>` one clip
//! - `POST /sync`        exchange events (see [`SyncRequest`])
//!
//! Progress is an append-only event log, so merging is just "add the events
//! you don't have and recompute".

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::db::{ClipExport, Db, Event, PhraseExport};

pub const UDP_PORT: u16 = 47811;
pub const HTTP_PORT: u16 = 47812;
const ASK: &str = "teach-me?";
const ANSWER: &str = "teach-me!";

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub device: String,
    pub phrases: Vec<PhraseExport>,
    pub clips: Vec<ClipExport>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncRequest {
    pub device: String,
    /// Events the server may not have yet.
    pub events: Vec<Event>,
    /// Latest event time the client knows per device.
    pub known: HashMap<String, DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncResponse {
    pub device: String,
    pub events: Vec<Event>,
}

/// What the phone needs to connect: shown as a QR code on the desktop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pairing {
    pub host: String,
    pub port: u16,
    pub key: String,
    /// Optional: API keys travel with the pairing so they need not be typed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_key: Option<String>,
}

/// Pairing key of this desktop; created on first use.
pub fn pairing_key(db: &Db) -> Result<String> {
    if let Some(k) = db.meta("pair_key")? {
        return Ok(k);
    }
    // 32 random-enough bytes from the OS: time, pid, stack address, hashed twice.
    let mut bytes = Vec::new();
    for i in 0..4u64 {
        let seed = format!("{:?}{}{:p}{i}", std::time::SystemTime::now(), std::process::id(), &bytes);
        bytes.extend_from_slice(crate::db::phrase_uid(&seed).as_bytes());
    }
    let key: String = bytes.iter().take(32).map(|b| char::from(b'a' + b % 26)).collect();
    db.set_meta("pair_key", &key)?;
    Ok(key)
}

/// LAN address of this machine (the one used to reach the internet).
pub fn local_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?; // no packet is sent
    s.local_addr().ok().map(|a| a.ip())
}

fn safe_clip_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 200
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.contains("..")
}

// ---------------------------------------------------------------- server

pub struct ServerConfig {
    pub db_path: PathBuf,
    pub clips_dir: PathBuf,
    pub udp_port: u16,
    pub http_port: u16,
}

/// Starts the UDP responder and the HTTP server on background threads.
/// Each opens its own SQLite connection (WAL allows it).
pub fn start_server(cfg: ServerConfig) -> Result<()> {
    let db = Db::open(&cfg.db_path)?;
    let key = pairing_key(&db)?;
    let device = db.device_id.clone();
    drop(db);

    // Right after a restart the previous process may still hold the ports.
    let mut attempt = 0;
    let (udp, http) = loop {
        let udp = UdpSocket::bind(("0.0.0.0", cfg.udp_port)).with_context(|| format!("UDP port {}", cfg.udp_port));
        let http = tiny_http::Server::http(("0.0.0.0", cfg.http_port))
            .map_err(|e| anyhow!("HTTP port {}: {e}", cfg.http_port));
        match (udp, http) {
            (Ok(u), Ok(h)) => break (u, h),
            (Err(e), _) | (_, Err(e)) if attempt >= 15 => return Err(e),
            _ => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    };

    let http_port = cfg.http_port;
    let answer_device = device.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        while let Ok((n, from)) = udp.recv_from(&mut buf) {
            let msg = String::from_utf8_lossy(&buf[..n]);
            if msg.starts_with(ASK) {
                let reply = format!("{ANSWER} {answer_device} {http_port}");
                let _ = udp.send_to(reply.as_bytes(), from);
            }
        }
    });

    std::thread::spawn(move || {
        let db = match Db::open(&cfg.db_path) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("sync server: {e}");
                return;
            }
        };
        for mut req in http.incoming_requests() {
            let resp = handle(&db, &cfg.clips_dir, &key, &mut req);
            let _ = match resp {
                Ok(r) => req.respond(r),
                Err((code, msg)) => req.respond(tiny_http::Response::from_string(msg).with_status_code(code)),
            };
        }
    });
    Ok(())
}

type HttpResponse = tiny_http::Response<Box<dyn Read + Send>>;

fn json_response<T: Serialize>(v: &T) -> Result<HttpResponse, (u16, String)> {
    let body = serde_json::to_vec(v).map_err(|e| (500, e.to_string()))?;
    let header = tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap();
    Ok(tiny_http::Response::new(200.into(), vec![header], Box::new(std::io::Cursor::new(body)) as Box<dyn Read + Send>, None, None))
}

fn handle(db: &Db, clips_dir: &Path, key: &str, req: &mut tiny_http::Request) -> Result<HttpResponse, (u16, String)> {
    let authorized = req
        .headers()
        .iter()
        .any(|h| h.field.equiv("Authorization") && h.value.as_str() == format!("Bearer {key}"));
    if !authorized {
        return Err((401, "pairing key required".into()));
    }
    let url = req.url().to_string();
    let internal = |e: anyhow::Error| (500, e.to_string());
    match (req.method(), url.as_str()) {
        (tiny_http::Method::Get, "/manifest") => json_response(&Manifest {
            device: db.device_id.clone(),
            phrases: db.export_phrases().map_err(internal)?,
            clips: db.export_clips().map_err(internal)?,
        }),
        (tiny_http::Method::Get, path) if path.starts_with("/clips/") => {
            let name = &path["/clips/".len()..];
            if !safe_clip_name(name) {
                return Err((400, "bad clip name".into()));
            }
            let file = std::fs::File::open(clips_dir.join(name)).map_err(|_| (404, "no such clip".into()))?;
            let len = file.metadata().ok().map(|m| m.len() as usize);
            let header = tiny_http::Header::from_bytes("Content-Type", "video/mp4").unwrap();
            Ok(tiny_http::Response::new(200.into(), vec![header], Box::new(file) as Box<dyn Read + Send>, len, None))
        }
        (tiny_http::Method::Post, "/sync") => {
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).map_err(|e| (400, e.to_string()))?;
            let sync: SyncRequest = serde_json::from_str(&body).map_err(|e| (400, e.to_string()))?;
            let reply = merge(db, &sync.events, &sync.known).map_err(internal)?;
            let _ = db.set_meta("sync_last_client", &format!("{} {}", sync.device, Utc::now().to_rfc3339()));
            json_response(&SyncResponse { device: db.device_id.clone(), events: reply })
        }
        _ => Err((404, "not found".into())),
    }
}

/// Adds incoming events, recomputes the affected phrases and returns the
/// events the other side lacks according to its watermarks.
fn merge(db: &Db, incoming: &[Event], known: &HashMap<String, DateTime<Utc>>) -> Result<Vec<Event>> {
    let mut touched = std::collections::HashSet::new();
    for e in incoming {
        if db.insert_event(e)? {
            touched.insert((e.phrase.clone(), e.dir));
        }
    }
    for (uid, dir) in touched {
        db.recompute(&uid, dir)?;
    }
    db.events_after(known)
}

// ---------------------------------------------------------------- client

/// Broadcasts a discovery request and returns the first desktop that answers.
/// `extra` targets are tried as well (last known address, manual IP).
pub fn discover(device: &str, udp_port: u16, extra: &[IpAddr], timeout: Duration) -> Result<Option<SocketAddr>> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_broadcast(true)?;
    sock.set_read_timeout(Some(Duration::from_millis(300)))?;
    let msg = format!("{ASK} {device}");
    let mut targets: Vec<IpAddr> = extra.to_vec();
    targets.push(IpAddr::V4(Ipv4Addr::BROADCAST));
    if let Some(IpAddr::V4(ip)) = local_ip() {
        // Subnet broadcast for a /24 network: some routers drop 255.255.255.255.
        let o = ip.octets();
        targets.push(IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], 255)));
    }
    let deadline = std::time::Instant::now() + timeout;
    let mut buf = [0u8; 512];
    while std::time::Instant::now() < deadline {
        for t in &targets {
            let _ = sock.send_to(msg.as_bytes(), (*t, udp_port));
        }
        let round_end = std::time::Instant::now() + Duration::from_millis(600);
        while std::time::Instant::now() < round_end {
            let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
            let reply = String::from_utf8_lossy(&buf[..n]);
            let mut parts = reply.split_whitespace();
            if parts.next() != Some(ANSWER) {
                continue;
            }
            let _server_device = parts.next();
            let port: u16 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(HTTP_PORT);
            return Ok(Some(SocketAddr::new(from.ip(), port)));
        }
    }
    Ok(None)
}

#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub new_phrases: usize,
    pub new_clips: usize,
    /// Clips the desktop could not serve; retried next time.
    pub failed_clips: usize,
    pub sent_events: usize,
    pub received_events: usize,
}

/// Pulls content and exchanges events with the desktop at `addr`.
pub fn sync_with(addr: SocketAddr, key: &str, db: &Db, clips_dir: &Path) -> Result<SyncReport> {
    let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(60)).build()?;
    let base = format!("http://{addr}");
    let auth = format!("Bearer {key}");
    let get = |path: &str| -> Result<reqwest::blocking::Response> {
        let r = http.get(format!("{base}{path}")).header("Authorization", &auth).send()?;
        if r.status() == 401 {
            bail!("ПК не принял ключ сопряжения — подключите телефон заново");
        }
        Ok(r.error_for_status()?)
    };
    let mut report = SyncReport::default();

    // 1. Content: phrases, then clips we don't have.
    let manifest: Manifest = get("/manifest")?.json()?;
    let known_uids: std::collections::HashSet<String> =
        db.export_phrases()?.into_iter().map(|p| p.uid).collect();
    for p in &manifest.phrases {
        if !known_uids.contains(&p.uid) {
            report.new_phrases += 1;
        }
        db.upsert_phrase(p)?;
    }
    std::fs::create_dir_all(clips_dir)?;
    for c in &manifest.clips {
        if db.has_clip(&c.file)? || !safe_clip_name(&c.file) {
            continue;
        }
        let path = clips_dir.join(&c.file);
        if !path.is_file() {
            // One bad clip must not stop the whole sync.
            let bytes = match get(&format!("/clips/{}", c.file)).and_then(|r| Ok(r.bytes()?)) {
                Ok(b) => b,
                Err(_) => {
                    report.failed_clips += 1;
                    continue;
                }
            };
            let tmp = path.with_extension("part");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &path)?;
        }
        db.insert_clip_export(c)?;
        report.new_clips += 1;
    }

    // 2. Events: send ours the server may lack, receive what we lack.
    let sent_mark = db.meta("sync_sent_until")?.and_then(|s| s.parse::<DateTime<Utc>>().ok());
    let mine: Vec<Event> = db
        .events_of_device(&db.device_id)?
        .into_iter()
        .filter(|e| sent_mark.is_none_or(|m| e.ts > m))
        .collect();
    let request = SyncRequest { device: db.device_id.clone(), events: mine.clone(), known: db.event_watermarks()? };
    let r = http.post(format!("{base}/sync")).header("Authorization", &auth).json(&request).send()?;
    let resp: SyncResponse = r.error_for_status()?.json()?;
    report.sent_events = mine.len();
    if let Some(last) = mine.iter().map(|e| e.ts).max() {
        db.set_meta("sync_sent_until", &last.to_rfc3339())?;
    }
    for e in &resp.events {
        if db.insert_event(e)? {
            report.received_events += 1;
        }
    }
    // Phrases may have arrived just now for events that were waiting.
    db.recompute_all()?;
    db.set_meta("sync_last_host", &addr.to_string())?;
    db.set_meta("sync_last_at", &Utc::now().to_rfc3339())?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{NewClip, NewPhrase};
    use crate::srs::{Direction, Outcome};

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "teachme-sync-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(d.join("clips")).unwrap();
        d
    }

    #[test]
    fn phone_and_desktop_sync_over_http() {
        // Desktop with two phrases and their clips.
        let pc_dir = temp_dir("pc");
        let pc = Db::open(&pc_dir.join("pc.db")).unwrap();
        for (i, pt) in ["Está bem.", "Não faz mal."].iter().enumerate() {
            let id = pc
                .insert_phrase(&NewPhrase { text_pt: pt, text_ru: "ru", alt_ru: &[], notes: "", cefr: "A2", usefulness: 3 })
                .unwrap();
            let file = format!("clip{i}.mp4");
            std::fs::write(pc_dir.join("clips").join(&file), format!("video {i}")).unwrap();
            pc.insert_clip(id, &NewClip { file: &file, source_url: "u", source_title: "t", start_ms: 0, end_ms: 1, subs_pt: pt })
                .unwrap();
        }
        let first = pc.phrase_exists("Está bem.").unwrap().unwrap();
        pc.record_outcome(first, Direction::PtToRu, Outcome::Failed, "", Utc::now() - chrono::Duration::hours(2))
            .unwrap();

        // Ephemeral ports so tests don't collide with a running app.
        let udp_port = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let http_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        start_server(ServerConfig {
            db_path: pc_dir.join("pc.db"),
            clips_dir: pc_dir.join("clips"),
            udp_port,
            http_port,
        })
        .unwrap();
        let key = pairing_key(&pc).unwrap();

        // Phone: empty database, finds the desktop and syncs.
        let phone_dir = temp_dir("phone");
        let phone = Db::open(&phone_dir.join("phone.db")).unwrap();
        let addr = discover(&phone.device_id, udp_port, &[IpAddr::V4(Ipv4Addr::LOCALHOST)], Duration::from_secs(3))
            .unwrap()
            .expect("desktop answers discovery");
        assert_eq!(addr.port(), http_port);

        let bad = sync_with(addr, "wrong-key", &phone, &phone_dir.join("clips"));
        assert!(bad.is_err(), "wrong pairing key is rejected");

        let r = sync_with(addr, &key, &phone, &phone_dir.join("clips")).unwrap();
        assert_eq!((r.new_phrases, r.new_clips, r.received_events), (2, 2, 1));
        assert_eq!(std::fs::read_to_string(phone_dir.join("clips/clip1.mp4")).unwrap(), "video 1");

        // The phone answers a card; the desktop gets it on the next sync.
        let on_phone = phone.phrase_exists("Está bem.").unwrap().unwrap();
        phone.record_outcome(on_phone, Direction::PtToRu, Outcome::First, "ok", Utc::now()).unwrap();
        let r = sync_with(addr, &key, &phone, &phone_dir.join("clips")).unwrap();
        assert_eq!((r.new_phrases, r.new_clips, r.sent_events, r.received_events), (0, 0, 1, 0));

        let due = |db: &Db| -> (String, i64) {
            db.conn
                .query_row(
                    "SELECT r.due, r.reps FROM reviews r JOIN phrases p ON p.id = r.phrase_id WHERE p.uid = ?1",
                    [crate::db::phrase_uid("Está bem.")],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        };
        // Server recomputes on merge; same schedule on both sides.
        let pc_view = Db::open(&pc_dir.join("pc.db")).unwrap();
        assert_eq!(due(&pc_view), due(&phone));
        assert_eq!(due(&phone).1, 2);
    }
}
