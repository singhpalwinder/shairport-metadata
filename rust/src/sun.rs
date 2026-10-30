//! Port of utils/sunSchedule.py. Sunrise/sunset come from Home Assistant's
//! sun.sun entity and are cached on disk (same JSON as the Python version);
//! HA is only asked again when the cache is older than MAX_AGE, and the cache
//! is used as-is when HA is unreachable.

use chrono::{DateTime, FixedOffset, Local, NaiveTime, TimeDelta, Timelike};
use serde::{Deserialize, Serialize};
use std::fs;
use std::time::Duration;

const MAX_AGE: TimeDelta = TimeDelta::hours(12);
const TIME_FMT: &str = "%H:%M:%S";

#[derive(Serialize, Deserialize)]
struct CacheFile {
    sunrise: String,
    sunset: String,
    last_checked: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Cache {
    sunrise: NaiveTime,
    sunset: NaiveTime,
    last_checked: DateTime<FixedOffset>,
}

pub struct SunSchedule {
    cache_path: String,
    ha_url: String,
    ha_token: Option<String>,
    cache: Option<Cache>,
    agent: ureq::Agent,
}

impl SunSchedule {
    pub fn new(cache_path: &str, ha_url: &str, ha_token: Option<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .into();
        let cache = load_cache(cache_path);
        Self {
            cache_path: cache_path.into(),
            ha_url: ha_url.trim_end_matches('/').into(),
            ha_token,
            cache,
            agent,
        }
    }

    fn save_cache(&self, c: &Cache) -> std::io::Result<()> {
        let f = CacheFile {
            sunrise: c.sunrise.format(TIME_FMT).to_string(),
            sunset: c.sunset.format(TIME_FMT).to_string(),
            last_checked: c.last_checked.format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
        };
        let tmp = format!("{}.tmp", self.cache_path);
        fs::write(&tmp, serde_json::to_string_pretty(&f).unwrap())?;
        fs::rename(&tmp, &self.cache_path)
    }

    fn fetch(&self) -> Result<Cache, String> {
        let token = self.ha_token.as_deref().ok_or("HA_TOKEN not set")?;
        let body = self
            .agent
            .get(format!("{}/api/states/sun.sun", self.ha_url))
            .header("Authorization", format!("Bearer {token}"))
            .call()
            .map_err(|e| e.to_string())?
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        parse_sun_state(&body, Local::now().fixed_offset())
    }

    /// Fetch from HA and update the cache. Returns true on success.
    pub fn refresh(&mut self) -> bool {
        match self.fetch() {
            Ok(c) => {
                if let Err(e) = self.save_cache(&c) {
                    eprintln!("Failed to write sun cache: {e}");
                }
                println!("🌅 Sun times updated: sunrise {}, sunset {}", c.sunrise, c.sunset);
                self.cache = Some(c);
                true
            }
            Err(e) => {
                eprintln!("Could not get sun times from Home Assistant: {e}; using cached times");
                false
            }
        }
    }

    pub fn is_dark(&mut self) -> bool {
        let now = Local::now().fixed_offset();
        if self.cache.is_none_or(|c| now - c.last_checked > MAX_AGE) {
            self.refresh();
        }
        match self.cache {
            Some(c) => dark_at(&c, now.time()),
            None => {
                // never fetched and HA unreachable: don't block the lights
                println!("No sun times available; allowing lights");
                true
            }
        }
    }
}

fn dark_at(c: &Cache, t: NaiveTime) -> bool {
    t >= c.sunset || t < c.sunrise
}

fn load_cache(path: &str) -> Option<Cache> {
    let parsed = fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str::<CacheFile>(&s).map_err(|e| e.to_string()))
        .and_then(|f| {
            Ok(Cache {
                sunrise: NaiveTime::parse_from_str(&f.sunrise, TIME_FMT).map_err(|e| e.to_string())?,
                sunset: NaiveTime::parse_from_str(&f.sunset, TIME_FMT).map_err(|e| e.to_string())?,
                last_checked: DateTime::parse_from_rfc3339(&f.last_checked).map_err(|e| e.to_string())?,
            })
        });
    match parsed {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("No usable sun cache at {path}: {e}");
            None
        }
    }
}

/// next_rising / next_setting are UTC timestamps; keep the local time-of-day.
fn parse_sun_state(body: &str, now: DateTime<FixedOffset>) -> Result<Cache, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let local = |k: &str| -> Result<NaiveTime, String> {
        let s = v["attributes"][k].as_str().ok_or(format!("missing {k}"))?;
        let t = DateTime::parse_from_rfc3339(s).map_err(|e| e.to_string())?;
        Ok(t.with_timezone(&Local).time().with_nanosecond(0).unwrap())
    };
    Ok(Cache {
        sunrise: local("next_rising")?,
        sunset: local("next_setting")?,
        last_checked: now.with_nanosecond(0).unwrap(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> NaiveTime {
        NaiveTime::parse_from_str(s, TIME_FMT).unwrap()
    }

    fn cache() -> Cache {
        Cache {
            sunrise: t("07:34:07"),
            sunset: t("18:36:20"),
            last_checked: DateTime::parse_from_rfc3339("2026-10-29T18:47:38-05:00").unwrap(),
        }
    }

    #[test]
    fn dark_window() {
        let c = cache();
        assert!(dark_at(&c, t("23:00:00")));
        assert!(dark_at(&c, t("05:00:00")));
        assert!(dark_at(&c, t("18:36:20")));
        assert!(!dark_at(&c, t("07:34:07")));
        assert!(!dark_at(&c, t("12:00:00")));
    }

    #[test]
    fn reads_python_cache_format() {
        let dir = std::env::temp_dir().join(format!("sun-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("sun.json");
        fs::write(&p, r#"{"sunrise": "07:34:07", "sunset": "18:36:20", "last_checked": "2026-10-29T18:47:38-05:00"}"#).unwrap();
        assert_eq!(load_cache(p.to_str().unwrap()), Some(cache()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_cache_used_when_ha_unreachable() {
        let dir = std::env::temp_dir().join(format!("sun-test2-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("sun.json");
        fs::write(&p, r#"{"sunrise": "00:00:00", "sunset": "00:00:00", "last_checked": "2000-01-01T00:00:00+00:00"}"#).unwrap();
        let mut s = SunSchedule::new(p.to_str().unwrap(), "http://127.0.0.1:9", Some("x".into()));
        // sunset == sunrise == midnight => always "dark"; proves the stale cache was used
        assert!(s.is_dark());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parses_ha_state() {
        let body = r#"{"attributes":{"next_rising":"2026-10-30T12:34:07.678236+00:00","next_setting":"2026-10-30T23:36:20.517058+00:00"}}"#;
        let c = parse_sun_state(body, cache().last_checked).unwrap();
        let rise = DateTime::parse_from_rfc3339("2026-10-30T12:34:07+00:00").unwrap().with_timezone(&Local).time();
        assert_eq!(c.sunrise, rise);
    }
}
