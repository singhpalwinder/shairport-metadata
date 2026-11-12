//! Spectrum bars for the matrix. Receives the copy of the playback stream that
//! `audio-tap` forwards, runs an FFT about 30 times a second, and sends 7 band levels
//! to the board over UDP: b"AL", count, then one byte (0..255) per band.

use realfft::{RealFftPlanner, RealToComplex};
use std::collections::VecDeque;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub const BANDS: usize = 7;
const FFT_SIZE: usize = 2048;
const TICK: Duration = Duration::from_millis(33);
const BOARD_PORT: u16 = 9091;
/// Bands are log-spaced between these frequencies.
const LOW_HZ: f32 = 50.0;
const HIGH_HZ: f32 = 14_000.0;
/// Each band is scaled against its own recent peak, so bass doesn't swamp the treble
/// and quiet tracks still move. The peak slowly relaxes so the range re-adapts.
const RANGE_DB: f32 = 36.0;
const PEAK_RELAX_DB_PER_TICK: f32 = 0.05;
const PEAK_FLOOR_DB: f32 = -50.0;
const SILENCE_DB: f32 = -75.0;
/// Stop sending once it has been silent this long, so the board can stop the record.
const QUIET_AFTER: Duration = Duration::from_secs(1);

/// When the tap last carried audible sound, in ms since `clock()` started (0 = never).
static LAST_HEARD_MS: AtomicU64 = AtomicU64::new(0);

fn clock() -> &'static Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now)
}

/// True if music was audible within `d`. Always false when the tap isn't installed.
pub fn heard_within(d: Duration) -> bool {
    let last = LAST_HEARD_MS.load(Ordering::Relaxed);
    last != 0 && (clock().elapsed().as_millis() as u64).saturating_sub(last) < d.as_millis() as u64
}

pub fn spawn(matrix_host: String) {
    let tap_port: u16 = env_or("AUDIO_TAP_PORT", 5555);
    // The tap sees audio as it's written to the DAC's buffer, slightly before you hear it.
    let delay = Duration::from_millis(env_or("AUDIO_DELAY_MS", 150));
    std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            if let Err(e) = run(&matrix_host, tap_port, delay) {
                eprintln!("audio: stopped: {e}");
            }
        })
        .expect("spawn audio thread");
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn run(matrix_host: &str, tap_port: u16, delay: Duration) -> std::io::Result<()> {
    let tap = UdpSocket::bind(("127.0.0.1", tap_port))?;
    tap.set_read_timeout(Some(Duration::from_millis(10)))?;
    let out = UdpSocket::bind("0.0.0.0:0")?;
    println!("audio: listening for the tap on 127.0.0.1:{tap_port}, bars to {matrix_host}:{BOARD_PORT}");

    let mut analyzer = Analyzer::new();
    let mut ring = Ring::new(FFT_SIZE);
    let mut buf = vec![0u8; 65536];
    let mut format: Option<(u32, u8, u8)> = None;
    let mut last_audio: Option<Instant> = None;
    let mut next_tick = Instant::now();
    let mut quiet_since: Option<Instant> = None;
    let mut queue: VecDeque<(Instant, [u8; BANDS])> = VecDeque::new();
    let mut board = Board::new(matrix_host);

    loop {
        match tap.recv(&mut buf) {
            Ok(n) => {
                if let Some(pkt) = Packet::parse(&buf[..n]) {
                    if format != Some((pkt.rate, pkt.channels, pkt.format)) {
                        format = Some((pkt.rate, pkt.channels, pkt.format));
                        println!("audio: tap stream {} Hz, {} ch, format {}", pkt.rate, pkt.channels, pkt.format);
                    }
                    pkt.mono_samples(|s| ring.push(s));
                    analyzer.set_rate(pkt.rate);
                    last_audio = Some(Instant::now());
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e),
        }

        let now = Instant::now();
        if now >= next_tick {
            next_tick = now + TICK;
            let fresh = last_audio.is_some_and(|t| now.duration_since(t) < Duration::from_millis(300));
            if fresh {
                let levels = analyzer.levels(&ring.snapshot());
                let loud = levels.iter().any(|&l| l > 0);
                if loud {
                    LAST_HEARD_MS.store((clock().elapsed().as_millis() as u64).max(1), Ordering::Relaxed);
                }
                quiet_since = if loud { None } else { quiet_since.or(Some(now)) };
                if quiet_since.is_none_or(|t| now.duration_since(t) < QUIET_AFTER) {
                    queue.push_back((now + delay, levels));
                }
            }
        }

        while queue.front().is_some_and(|(due, _)| *due <= now) {
            let (_, levels) = queue.pop_front().unwrap();
            board.send(&out, &encode(&levels));
        }
    }
}

/// Board address, re-resolved every 30 seconds in case its IP changes.
pub struct Board {
    host: String,
    addr: Option<SocketAddr>,
    resolved: Option<Instant>,
}

impl Board {
    pub fn new(host: &str) -> Self {
        Board { host: host.to_string(), addr: None, resolved: None }
    }

    pub fn send(&mut self, sock: &UdpSocket, packet: &[u8]) {
        if self.resolved.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) {
            self.resolved = Some(Instant::now());
            self.addr = (self.host.as_str(), BOARD_PORT).to_socket_addrs().ok().and_then(|mut a| a.next());
        }
        if let Some(addr) = self.addr {
            let _ = sock.send_to(packet, addr);
        }
    }
}

/// "Music is playing, but not through this Pi" (the Bose soundbar): the board spins
/// the record and shows the date where the bars would be. Sent about once a second;
/// the board drops back when it stops arriving.
pub const PLAYING_ELSEWHERE: &[u8] = b"NP";

pub fn encode(levels: &[u8; BANDS]) -> Vec<u8> {
    let mut p = vec![b'A', b'L', BANDS as u8];
    p.extend_from_slice(levels);
    p
}

/// One datagram from audio-tap.
pub struct Packet<'a> {
    pub rate: u32,
    pub channels: u8,
    pub format: u8,
    data: &'a [u8],
}

impl<'a> Packet<'a> {
    pub fn parse(b: &'a [u8]) -> Option<Self> {
        if b.len() < 10 || &b[..4] != b"PCM1" {
            return None;
        }
        let rate = u32::from_le_bytes(b[4..8].try_into().ok()?);
        let (channels, format) = (b[8].max(1), b[9]);
        Some(Packet { rate, channels, format, data: &b[10..] })
    }

    /// Calls `f` with each frame averaged down to one channel, scaled to -1..1.
    pub fn mono_samples(&self, mut f: impl FnMut(f32)) {
        let width = match self.format {
            0 => 2,
            1 => 3,
            2..=4 => 4,
            _ => return,
        };
        let ch = self.channels as usize;
        for frame in self.data.chunks_exact(width * ch) {
            let sum: f32 = frame.chunks_exact(width).map(|s| sample(self.format, s)).sum();
            f(sum / ch as f32);
        }
    }
}

fn sample(format: u8, s: &[u8]) -> f32 {
    match format {
        0 => i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
        1 => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
        2 => ((i32::from_le_bytes([s[0], s[1], s[2], s[3]]) << 8) >> 8) as f32 / 8_388_608.0,
        3 => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
        _ => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
    }
}

/// Fixed-size ring of the most recent mono samples.
struct Ring {
    data: Vec<f32>,
    pos: usize,
}

impl Ring {
    fn new(n: usize) -> Self {
        Ring { data: vec![0.0; n], pos: 0 }
    }
    fn push(&mut self, s: f32) {
        self.data[self.pos] = s;
        self.pos = (self.pos + 1) % self.data.len();
    }
    /// Oldest to newest.
    fn snapshot(&self) -> Vec<f32> {
        let mut v = Vec::with_capacity(self.data.len());
        v.extend_from_slice(&self.data[self.pos..]);
        v.extend_from_slice(&self.data[..self.pos]);
        v
    }
}

pub struct Analyzer {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    rate: u32,
    edges: [usize; BANDS + 1],
    peak_db: [f32; BANDS],
    last: [f32; BANDS],
}

impl Analyzer {
    pub fn new() -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT_SIZE);
        let window = (0..FFT_SIZE)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32).cos())
            .collect();
        let mut a = Analyzer { fft, window, rate: 0, edges: [0; BANDS + 1], peak_db: [PEAK_FLOOR_DB; BANDS], last: [0.0; BANDS] };
        a.set_rate(44_100);
        a
    }

    pub fn set_rate(&mut self, rate: u32) {
        if rate == self.rate || rate == 0 {
            return;
        }
        self.rate = rate;
        let bin_hz = rate as f32 / FFT_SIZE as f32;
        let max_bin = FFT_SIZE / 2;
        for i in 0..=BANDS {
            let hz = LOW_HZ * (HIGH_HZ / LOW_HZ).powf(i as f32 / BANDS as f32);
            self.edges[i] = ((hz / bin_hz).round() as usize).clamp(1, max_bin);
        }
        for i in 1..=BANDS {
            self.edges[i] = self.edges[i].max(self.edges[i - 1] + 1); // at least one bin per band
        }
    }

    /// Band levels 0..255 for the latest FFT_SIZE samples (oldest first).
    pub fn levels(&mut self, samples: &[f32]) -> [u8; BANDS] {
        let mut input: Vec<f32> = samples.iter().zip(&self.window).map(|(s, w)| s * w).collect();
        let mut spectrum = self.fft.make_output_vec();
        if self.fft.process(&mut input, &mut spectrum).is_err() {
            return [0; BANDS];
        }
        // A full-scale sine through a Hann window peaks at FFT_SIZE / 4.
        let scale = 4.0 / FFT_SIZE as f32;
        let mut out = [0u8; BANDS];
        for b in 0..BANDS {
            let bins = &spectrum[self.edges[b]..self.edges[b + 1].min(spectrum.len())];
            let power = bins.iter().map(|c| (c.norm() * scale).powi(2)).fold(0.0f32, f32::max);
            let db = 10.0 * (power + 1e-12).log10();

            self.peak_db[b] = (self.peak_db[b] - PEAK_RELAX_DB_PER_TICK).max(db).max(PEAK_FLOOR_DB);
            let mut level = if db < SILENCE_DB { 0.0 } else { ((db - (self.peak_db[b] - RANGE_DB)) / RANGE_DB).clamp(0.0, 1.0) };
            // Rise immediately, fall a little smoothed; the board adds its own fall-off.
            if level < self.last[b] {
                level = self.last[b] * 0.5 + level * 0.5;
            }
            self.last[b] = level;
            out[b] = (level * 255.0).round() as u8;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, rate: u32) -> Vec<f32> {
        (0..FFT_SIZE).map(|i| 0.5 * (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin()).collect()
    }

    #[test]
    fn a_tone_lights_its_own_band_most() {
        let mut a = Analyzer::new();
        let hz = 1000.0;
        let band = (0..BANDS).find(|&b| {
            let lo = a.edges[b] as f32 * 44_100.0 / FFT_SIZE as f32;
            let hi = a.edges[b + 1] as f32 * 44_100.0 / FFT_SIZE as f32;
            (lo..hi).contains(&hz)
        });
        let levels = a.levels(&tone(hz, 44_100));
        let loudest = (0..BANDS).max_by_key(|&b| levels[b]).unwrap();
        assert_eq!(Some(loudest), band, "{levels:?}");
        assert!(levels[loudest] > 200);
    }

    #[test]
    fn silence_is_zero() {
        let mut a = Analyzer::new();
        assert_eq!(a.levels(&vec![0.0; FFT_SIZE]), [0; BANDS]);
    }

    #[test]
    fn parses_tap_packets() {
        let mut b = b"PCM1".to_vec();
        b.extend_from_slice(&48_000u32.to_le_bytes());
        b.extend_from_slice(&[2, 0]);
        b.extend_from_slice(&i16::MAX.to_le_bytes()); // L
        b.extend_from_slice(&0i16.to_le_bytes()); // R
        let p = Packet::parse(&b).unwrap();
        assert_eq!((p.rate, p.channels, p.format), (48_000, 2, 0));
        let mut got = vec![];
        p.mono_samples(|s| got.push(s));
        assert_eq!(got.len(), 1);
        assert!((got[0] - 0.5).abs() < 0.001);
    }

    #[test]
    fn encodes_board_packet() {
        assert_eq!(encode(&[1, 2, 3, 4, 5, 6, 7]), b"AL\x07\x01\x02\x03\x04\x05\x06\x07".to_vec());
    }
}
