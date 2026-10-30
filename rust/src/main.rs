mod config;
mod image_proc;
mod lights;
mod matrix;
mod metadata;
mod sun;

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
    lights: Lights,
    sun: SunSchedule,
    http: ureq::Agent,
    matrix_host: String,
    track: TrackState,
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

    fn handle(&mut self, item: Item) {
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

            // stream end/flush, client disconnect, playback inactive: any ends the session
            ("ssnc", code @ ("pend" | "pfls" | "disc" | "aend")) => self.end_session(code),

            _ => {}
        }

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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("send") {
        let Some(path) = args.get(2) else {
            eprintln!("usage: shairport-metadata send <image>");
            std::process::exit(2);
        };
        let host = std::env::var("MATRIX_HOST").unwrap_or_else(|_| "matrix.lan".into());
        send_one(path, &host);
    }

    let cfg = Config::load().unwrap_or_else(|e| {
        eprintln!("config error: {e}");
        std::process::exit(1);
    });

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
    };

    let mut reader = MetadataReader::new(BufReader::with_capacity(1 << 20, pipe));
    let reason = loop {
        match reader.next_item() {
            Ok(Some(item)) => app.handle(item),
            Ok(None) => break "metadata pipe closed".to_string(),
            Err(e) => break format!("error reading metadata pipe: {e}"),
        }
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
