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
    sent: bool,
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
fn parse_copl(data: &[u8]) -> Option<(Option<String>, Option<Vec<u8>>)> {
    let pl = Value::from_reader(std::io::Cursor::new(data)).ok()?;
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
            self.track.sent = false; // new album → allow resend
        }
    }

    fn set_image(&mut self, art: Vec<u8>) {
        let h = hash(&art);
        if self.track.image_hash != Some(h) {
            println!("{{\"image\": \"data:{}\"}}", image_mime(&art));
            self.track.image = Some(art);
            self.track.image_hash = Some(h);
            self.track.sent = false; // image changed → resend
        }
    }

    fn handle(&mut self, item: Item) {
        match (item.typ.as_str(), item.code.as_str()) {
            ("core", "asal") => self.set_album(String::from_utf8_lossy(&item.data).into_owned()),

            // On AirPlay 2 shairport sends this instead of core/asal + ssnc/PICT
            ("ssnc", "copl") => {
                let Some((album, art)) = parse_copl(&item.data) else {
                    println!("copl: could not parse plist");
                    return;
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
            ("ssnc", code @ ("pend" | "pfls" | "disc" | "aend")) => {
                let t = &self.track;
                if self.lights.has_snapshot() || t.ready || t.album.is_some() || t.image.is_some() {
                    if self.lights.has_snapshot() {
                        println!("↩️ Restoring light states...");
                        self.lights.restore_states();
                    }
                    println!("🧼 Stream reset: {code}");
                    self.track = TrackState::default();
                    matrix::clear_artwork(&self.http, &self.matrix_host);
                    println!("{{}}");
                }
            }

            _ => {}
        }

        if self.track.ready && !self.track.sent && self.track.album.is_some() {
            if let Some(art) = self.track.image.take() {
                println!(
                    "📤 Processing artwork for {} ({} bytes, in-memory)...",
                    self.track.album.as_deref().unwrap_or(""),
                    art.len()
                );
                self.process_and_send(&art);
                self.track.image = Some(art);
                self.track.sent = true;
            }
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
            self.lights.set_color(ip.dominant_color(4));
        } else {
            println!("Sun is up; not changing lights");
        }

        let frame = image_proc::to_rgb565_be(&ip.enhance_image());
        match matrix::send_frame(&self.matrix_host, &frame) {
            Ok(()) => println!("Sent {} byte frame to matrix in {:?}", frame.len(), start.elapsed()),
            Err(e) => eprintln!("Failed to send image to matrix: {e}"),
        }
    }
}

fn main() {
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
    loop {
        match reader.next_item() {
            Ok(Some(item)) => app.handle(item),
            Ok(None) => {
                eprintln!("metadata pipe closed; exiting");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("error reading metadata pipe: {e}");
                std::process::exit(1);
            }
        }
    }
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
        let (album, art) = parse_copl(&buf).unwrap();
        assert_eq!(album.as_deref(), Some("Rodeo"));
        assert_eq!(art, Some(vec![0xff, 0xd8, 1]));
    }

    #[test]
    fn mime_sniffing() {
        assert_eq!(image_mime(b"\xff\xd8\xff"), "image/jpeg");
        assert_eq!(image_mime(b"\x89PNG\r\n\x1a\n...."), "image/png");
    }
}
