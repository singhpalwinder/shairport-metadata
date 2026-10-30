use std::env;

/// Same .env the Python version loads. Values already in the environment
/// (e.g. from systemd's EnvironmentFile) win.
pub const DOTENV_PATH: &str = "/opt/shairport-metadata/.env";

pub const MATRIX_PORT: u16 = 9090;
pub const SUN_CACHE_PATH: &str = "/opt/shairport-metadata/sun_times.json";

pub const LIGHT_TOPICS: &[&str] = &[
    "zigbee2mqtt/playbar1/set",
    "zigbee2mqtt/playbar2/set",
    "zigbee2mqtt/Desk/set",
    "zigbee2mqtt/bedLeft/set",
    "zigbee2mqtt/bedRight/set",
    "zigbee2mqtt/kitchenIsland1/set",
    "zigbee2mqtt/kitchenIsland2/set",
];

#[derive(Debug, Clone)]
pub struct Config {
    pub mqtt_username: String,
    pub mqtt_password: String,
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub ha_url: String,
    pub ha_token: Option<String>,
    pub pipe_path: String,
    pub matrix_host: String,
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let _ = dotenvy::from_path(DOTENV_PATH);

        let req = |k: &str| env::var(k).map_err(|_| format!("missing env var {k}"));
        let port = req("coordinatorPort")?;

        Ok(Config {
            mqtt_username: req("coordinatorUsername")?,
            mqtt_password: req("coordinatorPassword")?,
            mqtt_host: req("coordinatorURL")?,
            mqtt_port: port
                .trim()
                .parse()
                .map_err(|_| format!("bad coordinatorPort {port:?}"))?,
            ha_url: env::var("HA_URL").unwrap_or_else(|_| "http://homeassistant.lan:8123".into()),
            ha_token: env::var("HA_TOKEN").ok().filter(|t| !t.is_empty()),
            pipe_path: env::var("SSNC_PIPE_PATH").unwrap_or_else(|_| "/tmp/shairport-sync-metadata".into()),
            matrix_host: env::var("MATRIX_HOST").unwrap_or_else(|_| "matrix.lan".into()),
        })
    }
}
