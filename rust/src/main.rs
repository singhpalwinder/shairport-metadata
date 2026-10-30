mod config;
mod image_proc;
mod lights;
mod metadata;

use config::Config;
use metadata::MetadataReader;
use std::fs::File;
use std::io::BufReader;

fn main() {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(1);
        }
    };
    println!("loaded config for mqtt {}:{}", cfg.mqtt_host, cfg.mqtt_port);

    let pipe = File::open(config::SSNC_PIPE_PATH).expect("open metadata pipe");
    let mut reader = MetadataReader::new(BufReader::with_capacity(1 << 20, pipe));
    while let Ok(Some(item)) = reader.next_item() {
        println!("{}/{} ({} bytes)", item.typ, item.code, item.data.len());
    }
}
