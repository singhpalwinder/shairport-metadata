//! Matrix portal output: raw RGB565 frame over TCP, reset over HTTP.

use crate::config::MATRIX_PORT;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

pub fn send_frame(host: &str, frame: &[u8]) -> std::io::Result<()> {
    let addr = (host, MATRIX_PORT)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other(format!("{host} did not resolve")))?;
    let mut s = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    s.set_write_timeout(Some(TIMEOUT))?;
    s.set_nodelay(true)?;
    s.write_all(frame)
}

pub fn clear_artwork(agent: &ureq::Agent, host: &str) {
    println!("Clearing artwork...");
    match agent.post(format!("http://{host}/reset")).send_empty() {
        Ok(mut res) => {
            println!("{}", res.status());
            println!("{}", res.body_mut().read_to_string().unwrap_or_default());
        }
        Err(e) => eprintln!("there was an error sending reset command to matrix: {e}"),
    }
}
