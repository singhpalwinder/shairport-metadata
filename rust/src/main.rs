mod audio;
mod bose;
mod bose_auth;
mod config;
mod image_proc;
mod lights;
mod matrix;
mod metadata;
mod font5x7;
mod sun;
mod tile;
mod weather;

use config::Config;
use image_proc::ImageProcessor;
use lights::Lights;
use metadata::{Item, MetadataReader};
use plist::Value;
use std::fs::File;
use std::io::BufReader;
use std::time::{Duration, Instant};
use sun::SunSchedule;

const SNAPSHOT_TIMEOUT: Duration = Duration::from_millis(2500);
/// AirPlay 2 sometimes reports a client disconnect mid-song while audio keeps playing.
/// A disconnect only ends the session once the music has been silent this long.
const DISC_SILENCE: Duration = Duration::from_secs(3);
/// When AirPlay to the Pi ends while the soundbar is also playing (AirPlay to both),
/// wait this long for the soundbar to report the same pause before handing it the
/// session, so a normal pause doesn't flash the soundbar's copy of the cover.
const HANDOVER_WAIT: Duration = Duration::from_secs(2);
/// How often to tell the board music is playing on the soundbar (see audio::PLAYING_ELSEWHERE).
const ELSEWHERE_EVERY: Duration = Duration::from_secs(1);

/// Who the current session (matrix artwork + light colour) belongs to.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Source {
    /// shairport-sync on this Pi. Wins when both play: it also drives the spectrum bars.
    AirPlay,
    /// The Bose soundbar, for music sent straight to it.
    Soundbar,
}

/// Everything the main loop waits on.
enum Input {
    Pipe(Result<Item, String>),
    Bose(bose::Event),
}

#[derive(Default)]
struct TrackState {
    album: Option<String>,
    image: Option<Vec<u8>>,
    image_hash: Option<u64>,
    ready: bool,
    /// Hash of the artwork last sent to the matrix. Sending is keyed on the
    /// image changing, not the album: on a track change AirPlay 2 delivers the
    /// new album name with the *previous* artwork first, which used to flash
    /// the old cover (and re-set the lights) before the real one arrived.
    sent_hash: Option<u64>,
}

struct App {
    /// A disconnect that will end the session once the audio stops (see DISC_SILENCE).
    pending_disc: Option<Instant>,
    lights: Lights,
    sun: SunSchedule,
    http: ureq::Agent,
    matrix_host: String,
    track: TrackState,
    owner: Option<Source>,
    /// What the soundbar is playing right now (None when paused/stopped/off).
    bose_now: Option<bose::Track>,
    /// AirPlay ended while the soundbar was playing; see HANDOVER_WAIT.
    pending_handover: Option<Instant>,
    /// For the playing-elsewhere heartbeat to the board.
    udp: Option<std::net::UdpSocket>,
    board: audio::Board,
    last_elsewhere: Option<Instant>,
}

fn image_mime(data: &[u8]) -> &'static str {
    if data.starts_with(b"\xff\xd8") {
        "image/jpeg"
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else {
        "image/jpg"
    }
}

/// FNV-1a; only used to notice when artwork changes.
fn hash(data: &[u8]) -> u64 {
    data.iter()
        .fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// AirPlay 2 now-playing info: binary plist with params.params.{Album, ArtworkData}.
/// Ok(None) for updates that carry no now-playing info (e.g. play state only).
fn parse_copl(data: &[u8]) -> Result<Option<(Option<String>, Option<Vec<u8>>)>, plist::Error> {
    let pl = Value::from_reader(std::io::Cursor::new(data))?;
    Ok(now_playing(&pl))
}

fn now_playing(pl: &Value) -> Option<(Option<String>, Option<Vec<u8>>)> {
    let info = pl
        .as_dictionary()?
        .get("params")?
        .as_dictionary()?
        .get("params")?
        .as_dictionary()?;
    if info.is_empty() {
        return None;
    }
    let album = info
        .get("kMRMediaRemoteNowPlayingInfoAlbum")
        .and_then(Value::as_string)
        .map(String::from);
    let art = info
        .get("kMRMediaRemoteNowPlayingInfoArtworkData")
        .and_then(Value::as_data)
        .map(<[u8]>::to_vec);
    Some((album, art))
}

impl App {
    fn set_album(&mut self, album: String) {
        if self.track.album.as_deref() != Some(&album) {
            self.track.album = Some(album);
        }
    }

    fn set_image(&mut self, art: Vec<u8>) {
        let h = hash(&art);
        if self.track.image_hash != Some(h) {
            println!("{{\"image\": \"data:{}\"}}", image_mime(&art));
            self.track.image = Some(art);
            self.track.image_hash = Some(h);
        }
    }

    /// Runs about twice a second, between metadata items.
    fn tick(&mut self) {
        // No audio reaches the Pi when only the soundbar plays, so no spectrum packets
        // keep the record spinning: tell the board directly.
        if self.owner == Some(Source::Soundbar)
            && self.bose_now.is_some()
            && self.last_elsewhere.is_none_or(|t| t.elapsed() >= ELSEWHERE_EVERY)
        {
            self.last_elsewhere = Some(Instant::now());
            if let Some(sock) = &self.udp {
                self.board.send(sock, audio::PLAYING_ELSEWHERE);
            }
        }
        if let Some(since) = self.pending_disc {
            if since.elapsed() >= DISC_SILENCE && !audio::heard_within(DISC_SILENCE) {
                self.pending_disc = None;
                self.airplay_ended("disc");
            }
        }
        if let Some(since) = self.pending_handover {
            if since.elapsed() >= HANDOVER_WAIT {
                self.pending_handover = None;
                match self.bose_now.clone() {
                    Some(t) => {
                        println!("AirPlay to the Pi ended; following the soundbar");
                        self.show_soundbar(t);
                    }
                    None => self.end_session("airplay ended"),
                }
            }
        }
    }

    /// Make `src` the owner of the session. Switching keeps the lights snapshot,
    /// so the eventual restore still goes back to how they were before any music.
    fn claim(&mut self, src: Source) {
        if self.owner == Some(src) {
            return;
        }
        if let Some(prev) = self.owner {
            println!("Now following {src:?} (was {prev:?})");
            self.track = TrackState::default();
        }
        self.owner = Some(src);
    }

    /// shairport says its stream is over.
    fn airplay_ended(&mut self, reason: &str) {
        if self.owner == Some(Source::Soundbar) {
            return; // the session isn't AirPlay's
        }
        if self.bose_now.is_some() {
            self.pending_handover = Some(Instant::now());
            return;
        }
        self.end_session(reason);
    }

    fn handle_bose(&mut self, ev: bose::Event) {
        match ev {
            bose::Event::Show(t) => {
                self.bose_now = Some(t.clone());
                if self.owner != Some(Source::AirPlay) {
                    self.show_soundbar(t);
                }
            }
            bose::Event::Paused => self.bose_now = None,
            bose::Event::Stopped => {
                self.bose_now = None;
                if self.owner == Some(Source::Soundbar) {
                    self.end_session("soundbar stopped");
                }
            }
        }
    }

    fn show_soundbar(&mut self, t: bose::Track) {
        self.claim(Source::Soundbar);
        self.set_album(if t.album.is_empty() { t.title } else { t.album });
        self.set_image(t.art);
        self.track.ready = true;
        self.maybe_send();
    }

    fn handle(&mut self, item: Item) {
        // Any sign of an active stream cancels a pending disconnect, and puts the
        // session in AirPlay's hands even if the soundbar had it.
        if matches!(item.code.as_str(), "copl" | "pbeg" | "prsm" | "prgr" | "PICT") {
            self.pending_disc = None;
            self.pending_handover = None;
            self.claim(Source::AirPlay);
        }
        match (item.typ.as_str(), item.code.as_str()) {
            ("core", "asal") => self.set_album(String::from_utf8_lossy(&item.data).into_owned()),

            // On AirPlay 2 shairport sends this instead of core/asal + ssnc/PICT
            ("ssnc", "copl") => {
                let (album, art) = match parse_copl(&item.data) {
                    Ok(Some(v)) => v,
                    Ok(None) => return,
                    Err(e) => {
                        println!("copl: could not parse plist: {e}");
                        return;
                    }
                };
                if let Some(a) = album.filter(|a| !a.is_empty()) {
                    self.set_album(a);
                }
                if let Some(art) = art.filter(|a| !a.is_empty()) {
                    self.set_image(art);
                }
                self.track.ready = true; // a now-playing update means audio is active
            }

            ("ssnc", "PICT") => {
                if item.data.is_empty() {
                    println!("{{\"image\": \"\"}}");
                    return;
                }
                self.set_image(item.data);
            }

            ("ssnc", "pbeg" | "prsm" | "prgr") => {
                self.track.ready = true;
                if !self.lights.has_snapshot() && self.sun.is_dark() {
                    println!("🎛️ Snapshotting light states...");
                    self.lights.snapshot_states(SNAPSHOT_TIMEOUT);
                }
            }

            ("ssnc", "disc") => {
                if self.pending_disc.is_none() {
                    println!("Client disconnected; ending the session once the audio stops");
                }
                self.pending_disc = Some(Instant::now());
            }

            // stream end/flush, playback inactive: either ends the session
            ("ssnc", code @ ("pend" | "pfls" | "aend")) => {
                self.pending_disc = None;
                self.airplay_ended(code)
            }

            _ => {}
        }

        self.maybe_send();
    }

    /// Send the artwork (matrix + lights) once the session is live and the image is new.
    fn maybe_send(&mut self) {
        if self.track.ready
            && self.track.album.is_some()
            && self.track.image_hash.is_some()
            && self.track.image_hash != self.track.sent_hash
        {
            if let Some(art) = self.track.image.take() {
                println!(
                    "📤 Processing artwork for {} ({} bytes, in-memory)...",
                    self.track.album.as_deref().unwrap_or(""),
                    art.len()
                );
                self.process_and_send(&art);
                self.track.image = Some(art);
                self.track.sent_hash = self.track.image_hash;
            }
        }
    }

    /// Restore lights and clear the matrix; no-op if no session is active.
    fn end_session(&mut self, reason: &str) {
        self.owner = None;
        self.pending_handover = None;
        let t = &self.track;
        if self.lights.has_snapshot() || t.ready || t.album.is_some() || t.image.is_some() {
            if self.lights.has_snapshot() {
                println!("↩️ Restoring light states...");
                self.lights.restore_states();
            }
            println!("🧼 Stream reset: {reason}");
            self.track = TrackState::default();
            matrix::clear_artwork(&self.http, &self.matrix_host);
            println!("{{}}");
        }
    }

    fn process_and_send(&mut self, art: &[u8]) {
        let start = Instant::now();
        let ip = match ImageProcessor::from_bytes(art) {
            Ok(ip) => ip,
            Err(e) => {
                eprintln!("Failed to decode artwork ({} bytes): {e}", art.len());
                return;
            }
        };

        // lights first so they work even if the matrix send fails; only after dark
        if self.sun.is_dark() {
            // AirPlay 2 can deliver artwork before pbeg/prgr; snapshot before
            // the first colour change so restore gets the pre-session state
            if !self.lights.has_snapshot() {
                println!("🎛️ Snapshotting light states...");
                self.lights.snapshot_states(SNAPSHOT_TIMEOUT);
            }
            self.lights.set_color(ip.dominant_color(4));
        } else {
            println!("Sun is up; not changing lights");
        }

        let frame = ip.matrix_frame();
        match matrix::send_frame(&self.matrix_host, &frame) {
            Ok(()) => println!("Sent {} byte frame to matrix in {:?}", frame.len(), start.elapsed()),
            Err(e) => eprintln!("Failed to send image to matrix: {e}"),
        }
    }
}

/// `shairport-metadata send <image>`: push one cover to the matrix with the
/// current MATRIX_GAMMA / MATRIX_SATURATION, for tuning the look.
fn send_one(path: &str, matrix_host: &str) -> ! {
    let result = std::fs::read(path)
        .map_err(|e| e.to_string())
        .and_then(|b| ImageProcessor::from_bytes(&b))
        .and_then(|ip| matrix::send_frame(matrix_host, &ip.matrix_frame()).map_err(|e| e.to_string()));
    match result {
        Ok(()) => {
            println!("sent {path} to {matrix_host}");
            std::process::exit(0)
        }
        Err(e) => {
            eprintln!("send failed: {e}");
            std::process::exit(1)
        }
    }
}

/// `shairport-metadata bose-watch`: print what the soundbar reports, nothing else.
/// Needs only BOSE_HOST (and BOSE_TOKEN_PATH if the token isn't in the default place).
fn bose_watch() -> ! {
    let _ = dotenvy::from_path(config::DOTENV_PATH);
    let Ok(host) = std::env::var("BOSE_HOST") else {
        eprintln!("set BOSE_HOST to the soundbar's IP or hostname");
        std::process::exit(2);
    };
    let token = std::env::var("BOSE_TOKEN_PATH").unwrap_or_else(|_| config::BOSE_TOKEN_PATH.into());
    let (tx, rx) = std::sync::mpsc::channel();
    bose::spawn(host, token, move |ev| tx.send(ev).is_ok());
    for ev in rx {
        match ev {
            bose::Event::Show(t) => println!(
                "SHOW  [{}] {} - {} ({}) art: {} bytes, {}",
                t.source, t.title, t.artist, t.album, t.art.len(), image_mime(&t.art)
            ),
            other => println!("{other:?}"),
        }
    }
    std::process::exit(1)
}

/// `shairport-metadata bose-refresh`: refresh the soundbar token now (checks the
/// refresh path works without waiting hours for the token to near expiry).
fn bose_refresh() -> ! {
    let _ = dotenvy::from_path(config::DOTENV_PATH);
    let path = std::env::var("BOSE_TOKEN_PATH").unwrap_or_else(|_| config::BOSE_TOKEN_PATH.into());
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();
    let result = bose_auth::TokenFile::load(&path).and_then(|old| {
        println!("current token: {} min left", old.seconds_left() / 60);
        let new = bose_auth::refresh(&agent, &old).map_err(|e| e.to_string())?;
        new.save(&path).map_err(|e| format!("save {path}: {e}"))?;
        Ok(new)
    });
    match result {
        Ok(t) => {
            println!("refreshed: {} min left, saved to {path}", t.seconds_left() / 60);
            std::process::exit(0)
        }
        Err(e) => {
            eprintln!("refresh failed: {e}");
            std::process::exit(1)
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("weather-preview") {
        let path = args.get(2).map(String::as_str).unwrap_or("weather-preview.png");
        match weather::preview(path) {
            Ok(()) => println!("wrote {path}"),
            Err(e) => eprintln!("preview failed: {e}"),
        }
        std::process::exit(0);
    }
    if args.get(1).map(String::as_str) == Some("send") {
        let Some(path) = args.get(2) else {
            eprintln!("usage: shairport-metadata send <image>");
            std::process::exit(2);
        };
        let host = std::env::var("MATRIX_HOST").unwrap_or_else(|_| "matrix.lan".into());
        send_one(path, &host);
    }

    if args.get(1).map(String::as_str) == Some("bose-watch") {
        bose_watch();
    }
    if args.get(1).map(String::as_str) == Some("bose-refresh") {
        bose_refresh();
    }

    let cfg = Config::load().unwrap_or_else(|e| {
        eprintln!("config error: {e}");
        std::process::exit(1);
    });

    // Spectrum bars run on their own thread, fed by the ALSA tap (see README).
    audio::spawn(cfg.matrix_host.clone());
    // Weather for the idle clock, from Home Assistant.
    weather::spawn(cfg.ha_url.clone(), cfg.ha_token.clone(), cfg.matrix_host.clone());

    let pipe = File::open(&cfg.pipe_path).unwrap_or_else(|e| {
        eprintln!("cannot open {}: {e}", cfg.pipe_path);
        std::process::exit(1);
    });

    let mut app = App {
        lights: Lights::connect(&cfg),
        sun: SunSchedule::new(config::SUN_CACHE_PATH, &cfg.ha_url, cfg.ha_token.clone()),
        http: ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .into(),
        matrix_host: cfg.matrix_host.clone(),
        track: TrackState::default(),
        pending_disc: None,
        owner: None,
        bose_now: None,
        pending_handover: None,
        udp: std::net::UdpSocket::bind("0.0.0.0:0").inspect_err(|e| eprintln!("udp socket: {e}")).ok(),
        board: audio::Board::new(&cfg.matrix_host),
        last_elsewhere: None,
    };

    // Read the pipe on its own thread so the main loop can also run timers (tick).
    let (tx, rx) = std::sync::mpsc::channel::<Input>();
    match cfg.bose_host.clone() {
        Some(host) => {
            println!("Following Bose soundbar at {host}");
            let tx = tx.clone();
            bose::spawn(host, cfg.bose_token_path.clone(), move |ev| tx.send(Input::Bose(ev)).is_ok());
        }
        None => println!("BOSE_HOST not set; soundbar now-playing is off"),
    }
    std::thread::Builder::new()
        .name("metadata".into())
        .spawn(move || {
            let mut reader = MetadataReader::new(BufReader::with_capacity(1 << 20, pipe));
            loop {
                let msg = match reader.next_item() {
                    Ok(Some(item)) => Ok(item),
                    Ok(None) => Err("metadata pipe closed".to_string()),
                    Err(e) => Err(format!("error reading metadata pipe: {e}")),
                };
                let done = msg.is_err();
                if tx.send(Input::Pipe(msg)).is_err() || done {
                    return;
                }
            }
        })
        .expect("spawn metadata thread");

    let reason = loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Input::Pipe(Ok(item))) => app.handle(item),
            Ok(Input::Pipe(Err(reason))) => break reason,
            Ok(Input::Bose(ev)) => app.handle_bose(ev),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break "metadata reader stopped".to_string(),
        }
        app.tick();
    };

    // shairport-sync went away (crash/restart): don't leave the lights coloured
    eprintln!("{reason}; exiting");
    app.end_session("pipe closed");
    app.lights.shutdown(Duration::from_secs(3));
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copl_extracts_album_and_art() {
        let mut inner = plist::Dictionary::new();
        inner.insert("kMRMediaRemoteNowPlayingInfoAlbum".into(), Value::String("Rodeo".into()));
        inner.insert("kMRMediaRemoteNowPlayingInfoArtworkData".into(), Value::Data(vec![0xff, 0xd8, 1]));
        let mut params = plist::Dictionary::new();
        params.insert("params".into(), Value::Dictionary(inner));
        let mut root = plist::Dictionary::new();
        root.insert("params".into(), Value::Dictionary(params));

        let mut buf = Vec::new();
        Value::Dictionary(root).to_writer_binary(&mut buf).unwrap();
        let (album, art) = parse_copl(&buf).unwrap().unwrap();
        assert_eq!(album.as_deref(), Some("Rodeo"));
        assert_eq!(art, Some(vec![0xff, 0xd8, 1]));
    }

    #[test]
    fn mime_sniffing() {
        assert_eq!(image_mime(b"\xff\xd8\xff"), "image/jpeg");
        assert_eq!(image_mime(b"\x89PNG\r\n\x1a\n...."), "image/png");
    }
}
