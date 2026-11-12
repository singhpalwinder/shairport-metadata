//! Token for the Bose soundbar's local API.
//!
//! The soundbar only answers requests carrying a JWT ("control token") issued by
//! Bose's cloud for the account the speaker is set up with. It lasts 8 hours. To get
//! a new one without the password: trade the Azure AD B2C refresh token (Bose
//! accounts live in Azure B2C) for a fresh id_token, then trade that at Bose's
//! id service for a new control token. Same flow as the Bose app and pybose.
//!
//! The first token comes from an interactive login (`bose-login.py`); after that
//! this keeps it alive. Azure rotates the refresh token on every refresh, so the
//! file is rewritten each time and must stay writable.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const CLIENT_ID: &str = "e284648d-3009-47eb-8e74-670c5330ae54";
const B2C_TOKEN_URL: &str =
    "https://myboseid.bose.com/boseprodb2c.onmicrosoft.com/B2C_1A_MBI_SUSI/oauth2/v2.0/token";
const BOSE_TOKEN_URL: &str = "https://id.api.bose.io/id-jwt-core/idps/aad/B2C_1A_MBI_SUSI/token";
/// The Bose app's API key (public; it ships in the app and in pybose).
const BOSE_API_KEY: &str = "67616C617061676F732D70726F642D6D61647269642D696F73";
const USER_AGENT: &str = "Bose/37362 CFNetwork/3860.200.71 Darwin/25.1.0";

/// On-disk token, same layout `bose-login.py` writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenFile {
    pub access_token: String,
    #[serde(default)]
    pub bose_refresh_token: String,
    #[serde(default)]
    pub bose_person_id: String,
    #[serde(default)]
    pub azure_refresh_token: Option<String>,
}

#[derive(Debug)]
pub enum AuthError {
    /// Bose/Azure said no (expired or revoked refresh token): needs a new login.
    Rejected(String),
    /// Network trouble or a server error: try again later.
    Unavailable(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Rejected(m) => write!(f, "rejected: {m}"),
            AuthError::Unavailable(m) => write!(f, "unavailable: {m}"),
        }
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `exp` claim of a JWT, without checking the signature (the soundbar does that).
pub fn jwt_exp(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?.get("exp")?.as_u64()
}

impl TokenFile {
    pub fn load(path: &str) -> Result<Self, String> {
        let s = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        serde_json::from_str(&s).map_err(|e| format!("bad token file {path}: {e}"))
    }

    /// Write via a temp file + rename so a crash can't leave half a token behind.
    pub fn save(&self, path: &str) -> std::io::Result<()> {
        let tmp = format!("{path}.tmp");
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            let mut f = opts.open(&tmp)?;
            f.write_all(serde_json::to_string_pretty(self).unwrap().as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, Path::new(path))
    }

    pub fn expires_at(&self) -> Option<u64> {
        jwt_exp(&self.access_token)
    }

    /// Seconds of validity left (0 if expired or unreadable).
    pub fn seconds_left(&self) -> u64 {
        self.expires_at().map_or(0, |e| e.saturating_sub(now_unix()))
    }
}

fn classify(status: u16, what: &str, body: &str) -> AuthError {
    let body: String = body.chars().take(300).collect();
    let msg = format!("{what}: HTTP {status} {body}");
    if matches!(status, 400 | 401 | 403) {
        AuthError::Rejected(msg)
    } else {
        AuthError::Unavailable(msg)
    }
}

/// `agent` must be built with `http_status_as_error(false)` so error bodies can be logged.
pub fn refresh(agent: &ureq::Agent, tok: &TokenFile) -> Result<TokenFile, AuthError> {
    let azure_rt = tok
        .azure_refresh_token
        .as_deref()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AuthError::Rejected("token file has no azure_refresh_token".into()))?;

    // 1. Azure B2C: refresh token -> new id_token (+ rotated refresh token)
    let mut res = agent
        .post(B2C_TOKEN_URL)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "*/*")
        .header("Cache-Control", "no-cache")
        .send_form([("refresh_token", azure_rt), ("client_id", CLIENT_ID), ("grant_type", "refresh_token")])
        .map_err(|e| AuthError::Unavailable(format!("azure refresh: {e}")))?;
    let status = res.status().as_u16();
    let body = res.body_mut().read_to_string().unwrap_or_default();
    if status != 200 {
        return Err(classify(status, "azure refresh", &body));
    }
    let az: Value = serde_json::from_str(&body).map_err(|e| AuthError::Unavailable(format!("azure refresh: {e}")))?;
    let id_token = az["id_token"]
        .as_str()
        .ok_or_else(|| AuthError::Unavailable("azure refresh: no id_token in reply".into()))?;
    let new_azure_rt = az["refresh_token"].as_str().map(String::from).or_else(|| Some(azure_rt.to_string()));

    // 2. Bose id service: id_token -> control token
    let req = json!({
        "grant_type": "id_token",
        "id_token": id_token,
        "client_id": CLIENT_ID,
        "scope": format!("openid email profile offline_access {CLIENT_ID}"),
    });
    let mut res = agent
        .post(BOSE_TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("X-ApiKey", BOSE_API_KEY)
        .header("X-Api-Version", "1")
        .header("X-Software-Version", "1")
        .header("X-Library-Version", "1")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "*/*")
        .send(req.to_string())
        .map_err(|e| AuthError::Unavailable(format!("bose token exchange: {e}")))?;
    let status = res.status().as_u16();
    let body = res.body_mut().read_to_string().unwrap_or_default();
    if !matches!(status, 200 | 201) {
        return Err(classify(status, "bose token exchange", &body));
    }
    let b: Value = serde_json::from_str(&body).map_err(|e| AuthError::Unavailable(format!("bose token exchange: {e}")))?;
    let access_token = b["access_token"]
        .as_str()
        .filter(|t| jwt_exp(t).is_some())
        .ok_or_else(|| AuthError::Unavailable("bose token exchange: no usable access_token in reply".into()))?;

    Ok(TokenFile {
        access_token: access_token.to_string(),
        bose_refresh_token: b["refresh_token"].as_str().map_or_else(|| tok.bose_refresh_token.clone(), String::from),
        bose_person_id: b["bosePersonID"].as_str().map_or_else(|| tok.bose_person_id.clone(), String::from),
        azure_refresh_token: new_azure_rt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: &Value) -> String {
        let enc = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
        format!("{}.{}.sig", enc(&json!({"alg": "RS256"})), enc(claims))
    }

    #[test]
    fn reads_exp_claim() {
        assert_eq!(jwt_exp(&jwt(&json!({"exp": 1_800_000_000u64, "sub": "x"}))), Some(1_800_000_000));
        assert_eq!(jwt_exp("not-a-jwt"), None);
        assert_eq!(jwt_exp(&jwt(&json!({"sub": "no exp"}))), None);
    }

    #[test]
    fn token_file_roundtrip_and_python_layout() {
        // bose-login.py's output, which may have extra or missing keys
        let py = r#"{"access_token": "a.b.c", "bose_refresh_token": "r", "bose_person_id": "p",
                     "azure_refresh_token": "az"}"#;
        let t: TokenFile = serde_json::from_str(py).unwrap();
        assert_eq!(t.azure_refresh_token.as_deref(), Some("az"));

        let dir = std::env::temp_dir().join(format!("bose-token-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.json");
        let path = path.to_str().unwrap();
        t.save(path).unwrap();
        assert_eq!(TokenFile::load(path).unwrap(), t);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejections_vs_outages() {
        assert!(matches!(classify(400, "x", "invalid_grant"), AuthError::Rejected(_)));
        assert!(matches!(classify(503, "x", ""), AuthError::Unavailable(_)));
    }
}
