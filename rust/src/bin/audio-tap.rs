//! ALSA `file` plugin helper: copies the PCM that shairport-sync plays to the backend
//! over localhost UDP, for the matrix's spectrum bars.
//!
//! alsa-lib starts it as `audio-tap <rate> <channels> <format>` (see the README) and
//! writes the stream to its stdin. It must never block or exit early: playback waits on
//! this pipe, so send errors (backend not running) are ignored and the data dropped.
//!
//! Datagram: b"PCM1", rate u32 LE, channels u8, format u8, then whole frames.

use std::io::Read;
use std::net::UdpSocket;

pub const FORMATS: &[(&str, u8, usize)] = &[
    ("S16_LE", 0, 2),
    ("S24_3LE", 1, 3),
    ("S24_LE", 2, 4),
    ("S32_LE", 3, 4),
    ("FLOAT_LE", 4, 4),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rate: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(44100);
    let channels: u8 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2).max(1);
    let fmt = args.get(3).map(String::as_str).unwrap_or("S16_LE");
    let (code, width) = FORMATS
        .iter()
        .find(|(name, _, _)| *name == fmt)
        .map(|&(_, c, w)| (c, w))
        .unwrap_or((255, 2));
    let port: u16 = std::env::var("AUDIO_TAP_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5555);

    let sock = UdpSocket::bind("127.0.0.1:0").ok();
    let frame = width * channels as usize;
    let mut stdin = std::io::stdin().lock();
    let mut buf = vec![0u8; 8192];
    let mut have = 0usize;
    let mut packet = Vec::with_capacity(10 + buf.len());

    loop {
        let n = match stdin.read(&mut buf[have..]) {
            Ok(0) => return, // ALSA closed the stream
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        have += n;
        let whole = have / frame * frame;
        if whole == 0 {
            continue;
        }
        if let Some(sock) = &sock {
            packet.clear();
            packet.extend_from_slice(b"PCM1");
            packet.extend_from_slice(&rate.to_le_bytes());
            packet.push(channels);
            packet.push(code);
            packet.extend_from_slice(&buf[..whole]);
            let _ = sock.send_to(&packet, ("127.0.0.1", port));
        }
        buf.copy_within(whole..have, 0);
        have -= whole;
    }
}
