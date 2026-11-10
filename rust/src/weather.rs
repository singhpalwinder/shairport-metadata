//! Weather for the board's idle clock. Polls Home Assistant every 10 minutes (current
//! conditions, today's high/low, whether the sun is down), draws the tiles (tile.rs),
//! and keeps them on the board: once a minute it asks the board which clips it holds
//! (GET /clips) and pushes only the ones that are missing or out of date (POST /clip).

use crate::tile::{self, Canvas};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const FETCH_EVERY: Duration = Duration::from_secs(600);
const RETRY_AFTER: Duration = Duration::from_secs(60);
const SYNC_EVERY: Duration = Duration::from_secs(60);
/// Don't keep showing conditions this old if Home Assistant stops answering.
const MAX_AGE: Duration = Duration::from_secs(2 * 3600);
/// The board drops the tiles if this backend hasn't checked in for this long.
const TILE_TTL_S: u16 = 30 * 60;
const FRAME_MS: u16 = 100;
/// Clip slots on the board: the icon + temperature tile and the high/low row.
const SLOT_TOP: u8 = 0;
const SLOT_BOTTOM: u8 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct Weather {
    pub condition: u8,
    pub night: bool,
    pub temp: f64,
    pub high_low: Option<(f64, f64)>,
    pub celsius: bool,
}

/// Home Assistant condition names, in the order the tiles number them (0 = unknown).
const CONDITIONS: &[&[&str]] = &[
    &["sunny"],
    &["clear-night"],
    &["partlycloudy"],
    &["cloudy"],
    &["rainy"],
    &["pouring"],
    &["snowy"],
    &["snowy-rainy"],
    &["lightning"],
    &["lightning-rainy"],
    &["fog"],
    &["windy", "windy-variant"],
    &["hail"],
    &["exceptional"],
];

pub fn condition_code(state: &str) -> u8 {
    CONDITIONS.iter().position(|names| names.contains(&state)).map_or(0, |i| i as u8 + 1)
}

/// One looping clip for a board slot, encoded for POST /clip.
pub struct Clip {
    pub slot: u8,
    pub tag: u32,
    pub body: Vec<u8>,
}

impl Clip {
    pub fn new(slot: u8, x: u8, y: u8, frames: &[Canvas]) -> Self {
        let (w, h) = frames.first().map_or((0, 0), |c| (c.w as u8, c.h as u8));
        let pixels: Vec<u8> = frames.iter().flat_map(Canvas::rgb565_be).collect();
        let mut body = b"CLIP".to_vec();
        body.extend_from_slice(&[1, slot, x, y, w, h, frames.len() as u8]);
        body.extend_from_slice(&FRAME_MS.to_le_bytes());
        body.extend_from_slice(&TILE_TTL_S.to_le_bytes());
        let tag = fnv32(&body[5..]) ^ fnv32(&pixels);
        body.extend_from_slice(&tag.to_le_bytes());
        body.extend_from_slice(&pixels);
        Clip { slot, tag, body }
    }

    /// Empties a slot on the board.
    pub fn clear(slot: u8) -> Self {
        Clip::new(slot, 0, 0, &[])
    }
}

fn fnv32(data: &[u8]) -> u32 {
    data.iter().fold(0x811c9dc5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x01000193))
}

pub fn clips_for(w: &Weather) -> Vec<Clip> {
    vec![
        Clip::new(SLOT_TOP, 32, 0, &tile::top_frames(w)),
        Clip::new(SLOT_BOTTOM, 0, 23, &[tile::bottom_frame(w)]),
    ]
}

/// Parses GET /clips: "slot tag" per line.
pub fn parse_clip_status(body: &str) -> HashMap<u8, u32> {
    body.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect()
}

pub fn spawn(ha_url: String, ha_token: Option<String>, matrix_host: String) {
    let Some(token) = ha_token else {
        println!("weather: HA_TOKEN not set; the board will show the clock without weather");
        return;
    };
    let entity = std::env::var("WEATHER_ENTITY").ok().filter(|e| !e.is_empty());
    std::thread::Builder::new()
        .name("weather".into())
        .spawn(move || run(Ha::new(&ha_url, &token), entity, matrix_host))
        .expect("spawn weather thread");
}

fn run(ha: Ha, mut entity: Option<String>, matrix_host: String) {
    let board = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(5))).build().into();
    let mut current: Option<(Weather, Instant)> = None;
    let mut clips: Vec<Clip> = vec![];
    let mut next_fetch = Instant::now();
    let mut next_sync = Instant::now();

    loop {
        let now = Instant::now();
        if now >= next_fetch {
            if entity.is_none() {
                entity = ha.find_weather_entity().ok();
                match &entity {
                    Some(e) => println!("weather: using {e}"),
                    None => eprintln!("weather: no weather.* entity found in Home Assistant"),
                }
            }
            match entity.as_deref().map(|e| ha.fetch(e)) {
                Some(Ok(w)) => {
                    if current.as_ref().is_none_or(|(old, _)| *old != w) {
                        println!("weather: {w:?}");
                        clips = clips_for(&w);
                        next_sync = now; // push the change right away
                    }
                    current = Some((w, now));
                    next_fetch = now + FETCH_EVERY;
                }
                Some(Err(e)) => {
                    eprintln!("weather: {e}");
                    next_fetch = now + RETRY_AFTER;
                }
                None => next_fetch = now + RETRY_AFTER,
            }
        }
        if current.as_ref().is_some_and(|(_, at)| at.elapsed() > MAX_AGE) {
            current = None;
            clips.clear(); // stale: take the tiles off the board
        }
        if now >= next_sync {
            next_sync = now + SYNC_EVERY;
            if let Err(e) = sync(&board, &matrix_host, &clips) {
                eprintln!("weather: board: {e}");
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Pushes the clips the board is missing, and clears weather slots that shouldn't be there.
fn sync(agent: &ureq::Agent, host: &str, clips: &[Clip]) -> Result<(), String> {
    let status = agent
        .get(format!("http://{host}/clips"))
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let on_board = parse_clip_status(&status);

    let clears: Vec<Clip> = [SLOT_TOP, SLOT_BOTTOM]
        .into_iter()
        .filter(|s| on_board.contains_key(s) && !clips.iter().any(|c| c.slot == *s))
        .map(Clip::clear)
        .collect();
    for clip in clips.iter().filter(|c| on_board.get(&c.slot) != Some(&c.tag)).chain(&clears) {
        agent
            .post(format!("http://{host}/clip"))
            .header("Content-Type", "application/octet-stream")
            .send(&clip.body[..])
            .map_err(|e| format!("POST /clip slot {}: {e}", clip.slot))?;
        println!("weather: pushed slot {} ({} bytes)", clip.slot, clip.body.len());
    }
    Ok(())
}

/// `shairport-metadata weather-preview <out.png>`: every icon at four points in its loop,
/// plus the high/low row, scaled up, for checking the look without the board.
pub fn preview(path: &str) -> Result<(), String> {
    const S: u32 = 6;
    let cases: Vec<(u8, bool)> = (0..=14).map(|c| (c, false)).chain([(1, true), (3, true)]).collect();
    let (cell_w, cell_h) = (32 * S + S, 13 * S + S);
    let mut img = image::RgbImage::new(cell_w * 4 + 64 * 3, cell_h * cases.len() as u32);
    for (row, &(condition, night)) in cases.iter().enumerate() {
        let w = Weather { condition, night, temp: 59.6, high_low: Some((63.0, 48.0)), celsius: false };
        let frames = tile::top_frames(&w);
        for col in 0..4 {
            let c = &frames[col * frames.len() / 4];
            for y in 0..c.h {
                for x in 0..c.w {
                    for dy in 0..S - 1 {
                        for dx in 0..S - 1 {
                            let px = col as u32 * cell_w + x as u32 * S + dx;
                            img.put_pixel(px, row as u32 * cell_h + y as u32 * S + dy, image::Rgb(c.get(x, y)));
                        }
                    }
                }
            }
        }
        let b = tile::bottom_frame(&w);
        for y in 0..b.h {
            for x in 0..b.w {
                for dy in 0..2 {
                    for dx in 0..2 {
                        img.put_pixel(4 * cell_w + x as u32 * 3 + dx, row as u32 * cell_h + 20 + y as u32 * 3 + dy, image::Rgb(b.get(x, y)));
                    }
                }
            }
        }
    }
    img.save(path).map_err(|e| e.to_string())
}

struct Ha {
    url: String,
    token: String,
    agent: ureq::Agent,
}

impl Ha {
    fn new(url: &str, token: &str) -> Self {
        let agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().into();
        Ha { url: url.trim_end_matches('/').into(), token: token.into(), agent }
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        let body = self
            .agent
            .get(format!("{}{path}", self.url))
            .header("Authorization", format!("Bearer {}", self.token))
            .call()
            .map_err(|e| format!("GET {path}: {e}"))?
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        serde_json::from_str(&body).map_err(|e| e.to_string())
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let body = self
            .agent
            .post(format!("{}{path}", self.url))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| format!("POST {path}: {e}"))?
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        serde_json::from_str(&body).map_err(|e| e.to_string())
    }

    fn find_weather_entity(&self) -> Result<String, String> {
        let states = self.get("/api/states")?;
        states
            .as_array()
            .and_then(|a| a.iter().find_map(|s| s["entity_id"].as_str().filter(|id| id.starts_with("weather."))))
            .map(String::from)
            .ok_or_else(|| "no weather entity".into())
    }

    fn fetch(&self, entity: &str) -> Result<Weather, String> {
        let state = self.get(&format!("/api/states/{entity}"))?;
        let mut w = parse_state(&state).ok_or_else(|| format!("{entity} has no temperature yet"))?;
        // Today's high/low is optional; older weather integrations may not offer it.
        let body = serde_json::json!({ "entity_id": entity, "type": "daily" });
        if let Ok(resp) = self.post("/api/services/weather/get_forecasts?return_response", &body) {
            w.high_low = parse_today(&resp, entity);
        }
        if let Ok(sun) = self.get("/api/states/sun.sun") {
            w.night = sun["state"].as_str() == Some("below_horizon");
        }
        Ok(w)
    }
}

pub fn parse_state(v: &Value) -> Option<Weather> {
    let temp = v["attributes"]["temperature"].as_f64()?;
    let unit = v["attributes"]["temperature_unit"].as_str().unwrap_or("°F");
    Some(Weather {
        condition: condition_code(v["state"].as_str().unwrap_or("")),
        night: false,
        temp,
        high_low: None,
        celsius: unit.contains('C'),
    })
}

pub fn parse_today(v: &Value, entity: &str) -> Option<(f64, f64)> {
    let today = &v["service_response"][entity]["forecast"][0];
    Some((today["temperature"].as_f64()?, today["templow"].as_f64()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_conditions() {
        assert_eq!(condition_code("sunny"), 1);
        assert_eq!(condition_code("clear-night"), 2);
        assert_eq!(condition_code("windy-variant"), 12);
        assert_eq!(condition_code("exceptional"), 14);
        assert_eq!(condition_code("weird"), 0);
    }

    #[test]
    fn parses_home_assistant_responses() {
        let state = json!({"state": "partlycloudy", "attributes": {"temperature": 59.6, "temperature_unit": "°F"}});
        let mut w = parse_state(&state).unwrap();
        assert_eq!((w.condition, w.temp, w.celsius), (3, 59.6, false));

        let resp = json!({"service_response": {"weather.home": {"forecast": [
            {"temperature": 63.0, "templow": 48.0}, {"temperature": 70.0, "templow": 50.0}]}}});
        w.high_low = parse_today(&resp, "weather.home");
        assert_eq!(w.high_low, Some((63.0, 48.0)));
        assert_eq!(parse_today(&resp, "weather.other"), None);
    }

    #[test]
    fn clip_header_matches_the_board_protocol() {
        let w = Weather { condition: 5, night: true, temp: -3.6, high_low: Some((2.0, -9.4)), celsius: true };
        let clips = clips_for(&w);
        let top = &clips[0];
        assert_eq!(&top.body[..4], b"CLIP");
        assert_eq!(&top.body[4..11], &[1, SLOT_TOP, 32, 0, 32, 13, 16]);
        assert_eq!(top.body.len(), 19 + 16 * 32 * 13 * 2);
        assert_eq!(u32::from_le_bytes(top.body[15..19].try_into().unwrap()), top.tag);
        assert_eq!(Clip::clear(1).body.len(), 19);
        // same weather, same tag; different weather, different tag
        assert_eq!(clips_for(&w)[1].tag, clips[1].tag);
        assert_ne!(clips_for(&Weather { temp: 10.0, ..w })[0].tag, top.tag);
    }

    #[test]
    fn parses_board_clip_status() {
        let m = parse_clip_status("0 123\n1 456\ngarbage\n");
        assert_eq!(m.get(&0), Some(&123));
        assert_eq!(m.get(&1), Some(&456));
        assert_eq!(m.len(), 2);
    }
}
