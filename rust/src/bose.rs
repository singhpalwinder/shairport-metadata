//! Now playing from a Bose soundbar (Bose Music / "ECO2" products such as the Smart
//! Soundbars and the Lifestyle Ultra), so the matrix and lights also follow music
//! that goes straight to the soundbar (AirPlay to it, Spotify Connect, ...).
//!
//! The soundbar runs a WebSocket API on port 8082 (TLS with a self-signed device
//! cert, subprotocol "eco2"). Requests are JSON `{"header": {..., "token": <JWT>}, "body"}`.
//! We `PUT /subscription` for `/content/nowPlaying`; the soundbar then pushes a
//! `NOTIFY` with the full now-playing state on every change, over the same socket.
//! A `GET /content/nowPlaying` every minute doubles as keepalive and resync.
//!
//! Changes arrive in bursts (a skip gives PAUSED, a few half-empty states, BUFFERING
//! with the new title, PLAY), so `Tracker` waits for things to settle before
//! reporting, and only calls a pause a stop after STOP_GRACE.

use crate::bose_auth::{self, AuthError, TokenFile};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use serde_json::{Value, json};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

const PORT: u16 = 8082;
/// Wait this long after the last change before acting on it.
const SETTLE: Duration = Duration::from_millis(400);
/// Not playing for this long ends the session (a skip pauses for under a second).
const STOP_GRACE: Duration = Duration::from_secs(4);
/// Read timeout: how often the loop wakes up to run timers.
const TICK: Duration = Duration::from_millis(250);
const KEEPALIVE_EVERY: Duration = Duration::from_secs(60);
/// No traffic at all for this long (keepalive replies included): reconnect.
const DEAD_AFTER: Duration = Duration::from_secs(150);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Refresh the token when it has less than this left (it lasts 8h).
const REFRESH_BEFORE_S: u64 = 30 * 60;
/// Cover art is a few hundred KB at most.
const ART_LIMIT: u64 = 8 << 20;

/// What the main loop gets.
#[derive(Debug)]
pub enum Event {
    /// Playing (or about to): this track, with its cover.
    Show(Track),
    /// Stopped playing, may resume. The session ends with `Stopped` if it doesn't.
    Paused,
    Stopped,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub source: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art: Vec<u8>,
}

/// The parts of `/content/nowPlaying` we use.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NowPlaying {
    /// PLAY, PAUSED, BUFFERING, STOPPED, ...
    pub status: String,
    /// AIRPLAY, SPOTIFY, PRODUCT (TV/HDMI), ...
    pub source: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art_url: Option<String>,
}

impl NowPlaying {
    pub fn parse(body: &Value) -> Self {
        let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
        let md = &body["metadata"];
        let art = [&body["track"]["contentItem"]["containerArt"], &body["container"]["contentItem"]["containerArt"]]
            .into_iter()
            .find_map(|v| v.as_str().filter(|u| u.starts_with("http")).map(String::from));
        NowPlaying {
            status: s(&body["state"]["status"]),
            source: s(&body["source"]["sourceID"]),
            title: [&md["trackName"], &body["track"]["contentItem"]["name"]]
                .into_iter()
                .map(s)
                .find(|t| !t.is_empty())
                .unwrap_or_default(),
            artist: s(&md["artist"]),
            album: s(&md["album"]),
            art_url: art,
        }
    }

    /// Something worth showing: audio running (or about to) with a title and a cover.
    pub fn is_playing(&self) -> bool {
        matches!(self.status.as_str(), "PLAY" | "BUFFERING") && !self.title.is_empty() && self.art_url.is_some()
    }

    fn same_track(&self, o: &NowPlaying) -> bool {
        (&self.title, &self.artist, &self.album, &self.art_url) == (&o.title, &o.artist, &o.album, &o.art_url)
    }
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Show(NowPlaying),
    Paused,
    Stopped,
}

#[derive(Debug, Default)]
enum Shown {
    #[default]
    Nothing,
    Playing(NowPlaying),
    Paused,
}

/// Turns the stream of now-playing states into Show / Paused / Stopped.
#[derive(Debug, Default)]
pub struct Tracker {
    latest: Option<NowPlaying>,
    changed_at: Option<Instant>,
    stopped_since: Option<Instant>,
    shown: Shown,
}

impl Tracker {
    /// `None` means the soundbar is unreachable.
    pub fn update(&mut self, np: Option<NowPlaying>, now: Instant) {
        if np.as_ref().is_some_and(NowPlaying::is_playing) {
            self.stopped_since = None;
        } else if self.stopped_since.is_none() {
            self.stopped_since = Some(now);
        }
        if self.latest != np {
            self.latest = np;
            self.changed_at = Some(now);
        }
    }

    pub fn poll(&mut self, now: Instant) -> Option<Action> {
        let settled = self.changed_at.is_some_and(|t| now.duration_since(t) >= SETTLE);
        match &self.latest {
            Some(np) if np.is_playing() => {
                if !settled || matches!(&self.shown, Shown::Playing(p) if p.same_track(np)) {
                    return None;
                }
                self.shown = Shown::Playing(np.clone());
                Some(Action::Show(np.clone()))
            }
            _ => match self.shown {
                Shown::Playing(_) if settled => {
                    self.shown = Shown::Paused;
                    Some(Action::Paused)
                }
                Shown::Paused if self.stopped_since.is_some_and(|t| now.duration_since(t) >= STOP_GRACE) => {
                    self.shown = Shown::Nothing;
                    Some(Action::Stopped)
                }
                _ => None,
            },
        }
    }
}

/// Length of the JPEG at the start of `data`, or `data.len()` if it isn't one.
///
/// The soundbar serves AirPlay covers from a fixed 180 KB buffer: the JPEG, then
/// leftovers of earlier covers. Those would make the same cover hash differently
/// each time, so cut at the real end of image. Walks the segments rather than
/// searching for FFD9, since the leftovers contain old end-of-image markers too.
pub fn jpeg_len(data: &[u8]) -> usize {
    let whole = data.len();
    if !data.starts_with(&[0xFF, 0xD8]) {
        return whole;
    }
    let mut i = 2;
    loop {
        // marker: FF (fill FFs allowed) then the code
        if data.get(i) != Some(&0xFF) {
            return whole;
        }
        while data.get(i + 1) == Some(&0xFF) {
            i += 1;
        }
        let Some(&code) = data.get(i + 1) else { return whole };
        match code {
            0xD9 => return i + 2,
            0x01 | 0xD0..=0xD7 => i += 2,
            _ => {
                let Some(len) = data.get(i + 2..i + 4).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize) else {
                    return whole;
                };
                i += 2 + len;
                if code == 0xDA {
                    // entropy-coded data: runs to the next marker that isn't FF00 or a restart
                    loop {
                        match (data.get(i), data.get(i + 1)) {
                            (Some(0xFF), Some(0x00 | 0xD0..=0xD7)) => i += 2,
                            (Some(0xFF), Some(_)) => break,
                            (Some(_), _) => i += 1,
                            (None, _) => return whole,
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- connection

/// The soundbar's cert is signed by Bose's private device CA, so there is nothing
/// to verify it against. Accept the cert, but still check the handshake signatures.
#[derive(Debug)]
struct AcceptDeviceCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptDeviceCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

type Socket = WebSocket<StreamOwned<ClientConnection, TcpStream>>;

fn tls_config() -> Arc<ClientConfig> {
    let provider = Arc::new(ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptDeviceCert(provider)))
        .with_no_client_auth();
    Arc::new(cfg)
}

fn connect(host: &str, tls: &Arc<ClientConfig>) -> Result<Socket, String> {
    let addr = (host, PORT)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host} did not resolve"))?;
    let tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| format!("connect {addr}: {e}"))?;
    tcp.set_read_timeout(Some(CONNECT_TIMEOUT)).map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(CONNECT_TIMEOUT)).map_err(|e| e.to_string())?;
    tcp.set_nodelay(true).ok();

    let name = ServerName::try_from(host.to_string()).map_err(|e| format!("bad host {host}: {e}"))?;
    let conn = ClientConnection::new(tls.clone(), name).map_err(|e| format!("tls: {e}"))?;
    let stream = StreamOwned::new(conn, tcp);

    let mut req = format!("wss://{host}:{PORT}/").into_client_request().map_err(|e| e.to_string())?;
    req.headers_mut().insert("Sec-WebSocket-Protocol", "eco2".parse().unwrap());
    let (ws, _) = tungstenite::client(req, stream).map_err(|e| format!("websocket handshake: {e}"))?;
    ws.get_ref().sock.set_read_timeout(Some(TICK)).map_err(|e| e.to_string())?;
    Ok(ws)
}

fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

enum SessionEnd {
    /// The token is about to expire: refresh it and reconnect.
    TokenExpiring,
    /// The soundbar rejected the token.
    Unauthorized(String),
    Failed(String),
}

struct Client {
    host: String,
    token_path: String,
    /// Hands events to the main loop; false once it has gone away.
    emit: Box<dyn FnMut(Event) -> bool + Send>,
    tls: Arc<ClientConfig>,
    /// For cover art (from the soundbar, or the streaming service's CDN).
    http: ureq::Agent,
    /// For the token refresh; error statuses are returned, not raised, so we can log them.
    auth_http: ureq::Agent,
    tracker: Tracker,
    art_cache: Option<(String, Vec<u8>)>,
    /// An Azure refresh token that was rejected; don't keep hammering Bose with it.
    rejected_refresh: Option<String>,
    name: Option<String>,
}

impl Client {
    /// Run the tracker's timers and forward what it decides to the main loop.
    fn step(&mut self) {
        while let Some(action) = self.tracker.poll(Instant::now()) {
            let event = match action {
                Action::Show(np) => match self.fetch_art(np.art_url.as_deref().unwrap_or_default()) {
                    Ok(art) => {
                        println!("🔊 Soundbar ({}): {} - {}", np.source, np.title, np.artist);
                        Event::Show(Track { source: np.source, title: np.title, artist: np.artist, album: np.album, art })
                    }
                    Err(e) => {
                        eprintln!("bose: could not fetch artwork: {e}");
                        continue;
                    }
                },
                Action::Paused => Event::Paused,
                Action::Stopped => {
                    println!("🔊 Soundbar stopped");
                    Event::Stopped
                }
            };
            if !(self.emit)(event) {
                std::process::exit(0); // main loop is gone
            }
        }
    }

    fn fetch_art(&mut self, url: &str) -> Result<Vec<u8>, String> {
        if let Some((u, art)) = &self.art_cache {
            if u == url {
                return Ok(art.clone());
            }
        }
        let mut art = self
            .http
            .get(url)
            .call()
            .and_then(|mut r| r.body_mut().with_config().limit(ART_LIMIT).read_to_vec())
            .map_err(|e| format!("{url}: {e}"))?;
        art.truncate(jpeg_len(&art));
        self.art_cache = Some((url.to_string(), art.clone()));
        Ok(art)
    }

    /// Sleep for `d` while still running the tracker's timers.
    fn idle(&mut self, d: Duration) {
        let until = Instant::now() + d;
        while Instant::now() < until {
            std::thread::sleep(TICK);
            self.step();
        }
    }

    /// A token with some life left, refreshing it first if it's close to expiry.
    /// Err(wait) if there is none; the caller retries after `wait`.
    fn token(&mut self, force_refresh: bool) -> Result<TokenFile, Duration> {
        let tok = TokenFile::load(&self.token_path).map_err(|e| {
            eprintln!("bose: {e}");
            Duration::from_secs(300)
        })?;
        let left = tok.seconds_left();
        if !force_refresh && left > REFRESH_BEFORE_S {
            return Ok(tok);
        }
        if tok.azure_refresh_token.is_some() && tok.azure_refresh_token == self.rejected_refresh {
            // Already told the user; wait for a new token file.
            return if left > 60 { Ok(tok) } else { Err(Duration::from_secs(300)) };
        }
        match bose_auth::refresh(&self.auth_http, &tok) {
            Ok(new) => {
                if let Err(e) = new.save(&self.token_path) {
                    eprintln!("bose: could not save refreshed token to {}: {e}", self.token_path);
                }
                println!("bose: token refreshed, valid for {} min", new.seconds_left() / 60);
                Ok(new)
            }
            Err(e) => {
                if let AuthError::Rejected(_) = e {
                    eprintln!(
                        "bose: ❌ token refresh {e}. Log in again with bose-login.py and copy the token to {}. \
                         Soundbar now-playing is off until then.",
                        self.token_path
                    );
                    self.rejected_refresh = tok.azure_refresh_token.clone();
                } else {
                    eprintln!("bose: token refresh failed, will retry: {e}");
                }
                if left > 60 { Ok(tok) } else { Err(Duration::from_secs(60)) }
            }
        }
    }

    fn request(ws: &mut Socket, n: &mut u64, device: &str, token: &str, method: &str, resource: &str, body: Value, version: u8) -> tungstenite::Result<()> {
        *n += 1;
        let msg = json!({
            "header": {"device": device, "method": method, "msgtype": "REQUEST", "reqID": *n,
                       "resource": resource, "status": 200, "token": token, "version": version},
            "body": body,
        });
        ws.send(Message::text(msg.to_string()))
    }

    /// One connection: subscribe, then follow notifications until something ends it.
    fn session(&mut self, token: &TokenFile) -> SessionEnd {
        let mut ws = match connect(&self.host, &self.tls) {
            Ok(ws) => ws,
            Err(e) => return SessionEnd::Failed(e),
        };
        let started = Instant::now();
        let mut device: Option<String> = None;
        let mut subscribed = false;
        let mut req_id = 0u64;
        let mut last_rx = Instant::now();
        let mut last_keepalive = Instant::now();

        loop {
            match ws.read() {
                Ok(Message::Text(text)) => {
                    last_rx = Instant::now();
                    let Ok(m) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                    let h = &m["header"];
                    if device.is_none() {
                        device = h["device"].as_str().map(String::from);
                    }
                    let status = h["status"].as_i64().unwrap_or(200);
                    let response = h["msgtype"].as_str() == Some("RESPONSE");
                    if response && status == 401 {
                        return SessionEnd::Unauthorized(m["error"]["message"].as_str().unwrap_or("401").to_string());
                    }
                    match h["resource"].as_str() {
                        Some("/content/nowPlaying") if status == 200 => {
                            self.tracker.update(Some(NowPlaying::parse(&m["body"])), Instant::now());
                        }
                        Some("/system/info") if response && status == 200 => {
                            let name = m["body"]["name"].as_str().unwrap_or("soundbar").to_string();
                            if self.name.as_ref() != Some(&name) {
                                println!("bose: connected to {name} ({})", m["body"]["productName"].as_str().unwrap_or("?"));
                                self.name = Some(name);
                            }
                        }
                        Some(r) if response && status != 200 => eprintln!("bose: {r} failed: {}", m["error"]),
                        _ => {}
                    }
                }
                Ok(Message::Close(_)) => return SessionEnd::Failed("soundbar closed the connection".into()),
                Ok(_) => last_rx = Instant::now(),
                Err(e) if is_timeout(&e) => {}
                Err(e) => return SessionEnd::Failed(e.to_string()),
            }

            if !subscribed {
                let Some(dev) = device.clone() else {
                    if started.elapsed() > CONNECT_TIMEOUT {
                        return SessionEnd::Failed("soundbar never said hello".into());
                    }
                    continue;
                };
                let resources = json!({"notifications": [{"resource": "/content/nowPlaying", "version": 1}]});
                let t = &token.access_token;
                let sent = Self::request(&mut ws, &mut req_id, &dev, t, "PUT", "/subscription", resources, 2)
                    .and_then(|_| Self::request(&mut ws, &mut req_id, &dev, t, "GET", "/system/info", json!({}), 1))
                    .and_then(|_| Self::request(&mut ws, &mut req_id, &dev, t, "GET", "/content/nowPlaying", json!({}), 1));
                if let Err(e) = sent {
                    return SessionEnd::Failed(e.to_string());
                }
                subscribed = true;
            }

            self.step();

            if last_keepalive.elapsed() >= KEEPALIVE_EVERY {
                last_keepalive = Instant::now();
                let dev = device.as_deref().unwrap_or_default();
                if let Err(e) = Self::request(&mut ws, &mut req_id, dev, &token.access_token, "GET", "/content/nowPlaying", json!({}), 1) {
                    return SessionEnd::Failed(e.to_string());
                }
            }
            if last_rx.elapsed() >= DEAD_AFTER {
                return SessionEnd::Failed(format!("nothing from the soundbar for {DEAD_AFTER:?}"));
            }
            if token.seconds_left() <= REFRESH_BEFORE_S {
                let _ = ws.close(None);
                return SessionEnd::TokenExpiring;
            }
        }
    }

    fn run(mut self) {
        let mut backoff = Duration::from_secs(2);
        let mut force_refresh = false;
        let mut failing = false;
        loop {
            let token = match self.token(force_refresh) {
                Ok(t) => t,
                Err(wait) => {
                    self.tracker.update(None, Instant::now());
                    self.idle(wait);
                    continue;
                }
            };
            force_refresh = false;
            let connected_at = Instant::now();
            let end = self.session(&token);
            if connected_at.elapsed() > Duration::from_secs(30) {
                backoff = Duration::from_secs(2); // it was working
            }
            match end {
                SessionEnd::TokenExpiring => continue,
                SessionEnd::Unauthorized(msg) if msg.to_lowercase().contains("not before") => {
                    // fresh token, but the soundbar's clock is a little behind
                    self.idle(Duration::from_secs(2));
                }
                SessionEnd::Unauthorized(msg) => {
                    eprintln!("bose: soundbar rejected the token ({msg}); refreshing");
                    force_refresh = true;
                    self.idle(backoff);
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                SessionEnd::Failed(e) => {
                    if !failing || backoff >= MAX_BACKOFF {
                        eprintln!("bose: {e}; reconnecting");
                    }
                    failing = true;
                    self.tracker.update(None, Instant::now());
                    self.idle(backoff);
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            }
            failing = false;
        }
    }
}

/// Follow the soundbar at `host` on a background thread, passing events to `emit`.
pub fn spawn(host: String, token_path: String, emit: impl FnMut(Event) -> bool + Send + 'static) {
    let agent = |status_errors: bool| -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .http_status_as_error(status_errors)
            .build()
            .into()
    };
    let client = Client {
        host,
        token_path,
        emit: Box::new(emit),
        tls: tls_config(),
        http: agent(true),
        auth_http: agent(false),
        tracker: Tracker::default(),
        art_cache: None,
        rejected_refresh: None,
        name: None,
    };
    std::thread::Builder::new()
        .name("bose".into())
        .spawn(move || client.run())
        .expect("spawn bose thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real Lifestyle Ultra capture (AirPlay from an iPhone).
    fn body(status: &str, title: &str, art: &str) -> Value {
        json!({
            "container": {"contentItem": {"containerArt": art, "source": "AIRPLAY"}},
            "metadata": {"album": if title.is_empty() { "" } else { "Moosetape" },
                         "artist": if title.is_empty() { "" } else { "Sidhu Moose Wala" },
                         "duration": 225, "trackName": title},
            "source": {"sourceDisplayName": "AirPlay", "sourceID": "AIRPLAY"},
            "state": {"status": status, "timeIntoTrack": 48},
            "track": {"contentItem": {"containerArt": art, "name": title, "source": "AIRPLAY"}},
        })
    }

    const ART1: &str = "http://192.168.4.9/AirPlay2/ap2_01794448042.jpg";
    const ART2: &str = "http://192.168.4.9/AirPlay2/ap2_01794448272.jpg";

    #[test]
    fn cuts_leftovers_after_the_jpeg() {
        let img = image::RgbImage::from_fn(64, 64, |x, y| image::Rgb([(x * 4) as u8, (y * 4) as u8, 90]));
        let mut jpg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 85).encode_image(&img).unwrap();
        assert_eq!(jpeg_len(&jpg), jpg.len());

        // the soundbar's buffer: this cover, then the tail of an older, longer one
        let mut older = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut older, 100)
            .encode_image(&image::RgbImage::from_fn(128, 128, |x, y| image::Rgb([(x ^ y) as u8, 7, (x * y) as u8])))
            .unwrap();
        let mut buf = jpg.clone();
        buf.extend_from_slice(&older[jpg.len().min(older.len())..]);
        buf.extend_from_slice(&older);
        assert_eq!(jpeg_len(&buf), jpg.len());
        assert!(image::load_from_memory(&buf[..jpeg_len(&buf)]).is_ok());

        // not a JPEG, or cut short: leave it alone
        assert_eq!(jpeg_len(b"\x89PNG\r\n\x1a\n"), 8);
        assert_eq!(jpeg_len(&jpg[..jpg.len() / 2]), jpg.len() / 2);
    }

    #[test]
    fn parses_airplay_now_playing() {
        let np = NowPlaying::parse(&body("PLAY", "Invincible", ART1));
        assert_eq!(np.source, "AIRPLAY");
        assert_eq!(np.title, "Invincible");
        assert_eq!(np.album, "Moosetape");
        assert_eq!(np.art_url.as_deref(), Some(ART1));
        assert!(np.is_playing());
        // mid-skip states: no title or no art
        assert!(!NowPlaying::parse(&body("BUFFERING", "", ART2)).is_playing());
        assert!(!NowPlaying::parse(&body("PLAY", "Low", "")).is_playing());
        assert!(!NowPlaying::parse(&body("PAUSED", "Invincible", ART1)).is_playing());
        // TV input: no metadata at all
        assert!(!NowPlaying::parse(&json!({"state": {"status": "PLAY"}, "source": {"sourceID": "PRODUCT"}})).is_playing());
    }

    fn feed(t: &mut Tracker, at: Instant, b: Value) {
        t.update(Some(NowPlaying::parse(&b)), at);
    }

    fn titles(a: Option<Action>) -> Option<String> {
        match a {
            Some(Action::Show(np)) => Some(np.title),
            Some(Action::Paused) => Some("<paused>".into()),
            Some(Action::Stopped) => Some("<stopped>".into()),
            None => None,
        }
    }

    #[test]
    fn a_skip_burst_shows_only_the_new_track() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut t = Tracker::default();
        feed(&mut t, ms(0), body("PLAY", "Invincible", ART1));
        assert_eq!(titles(t.poll(ms(100))), None, "waits to settle");
        assert_eq!(titles(t.poll(ms(500))), Some("Invincible".into()));
        assert_eq!(titles(t.poll(ms(900))), None, "shown once");

        // the burst the soundbar sent for one skip, all within ~1s
        feed(&mut t, ms(9000), body("PAUSED", "Invincible", ART1));
        feed(&mut t, ms(9050), body("PAUSED", "", ""));
        feed(&mut t, ms(9100), body("BUFFERING", "", ""));
        feed(&mut t, ms(9150), body("BUFFERING", "", ART2));
        assert_eq!(titles(t.poll(ms(9200))), None, "nothing mid-burst");
        feed(&mut t, ms(9300), body("BUFFERING", "Low", ART2));
        feed(&mut t, ms(9800), body("PLAY", "Low", ART2));
        assert_eq!(titles(t.poll(ms(10_300))), Some("Low".into()));
        assert_eq!(titles(t.poll(ms(20_000))), None);
    }

    #[test]
    fn pause_then_stop_after_grace_unless_resumed() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut t = Tracker::default();
        feed(&mut t, ms(0), body("PLAY", "Low", ART2));
        assert!(t.poll(ms(500)).is_some());

        feed(&mut t, ms(1000), body("PAUSED", "Low", ART2));
        assert_eq!(titles(t.poll(ms(1500))), Some("<paused>".into()));
        // resume within the grace: shown again (the main loop dedupes identical art)
        feed(&mut t, ms(3000), body("PLAY", "Low", ART2));
        assert_eq!(titles(t.poll(ms(3500))), Some("Low".into()));

        feed(&mut t, ms(10_000), body("PAUSED", "Low", ART2));
        assert_eq!(titles(t.poll(ms(10_500))), Some("<paused>".into()));
        assert_eq!(titles(t.poll(ms(13_000))), None);
        assert_eq!(titles(t.poll(ms(14_100))), Some("<stopped>".into()));
        assert_eq!(titles(t.poll(ms(20_000))), None);
    }

    #[test]
    fn losing_the_soundbar_counts_as_stopping() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut t = Tracker::default();
        feed(&mut t, ms(0), body("PLAY", "Low", ART2));
        assert!(t.poll(ms(500)).is_some());
        t.update(None, ms(1000));
        assert_eq!(titles(t.poll(ms(1500))), Some("<paused>".into()));
        assert_eq!(titles(t.poll(ms(5100))), Some("<stopped>".into()));
    }

    #[test]
    fn keepalive_repeats_dont_delay_or_reshow() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut t = Tracker::default();
        feed(&mut t, ms(0), body("PLAY", "Low", ART2));
        assert!(t.poll(ms(500)).is_some());
        feed(&mut t, ms(60_000), body("PLAY", "Low", ART2));
        assert_eq!(titles(t.poll(ms(60_100))), None);
        // a paused soundbar answering keepalives still stops on time
        feed(&mut t, ms(61_000), body("PAUSED", "Low", ART2));
        assert!(t.poll(ms(61_500)).is_some());
        feed(&mut t, ms(63_000), body("PAUSED", "Low", ART2));
        assert_eq!(titles(t.poll(ms(65_100))), Some("<stopped>".into()));
    }
}
