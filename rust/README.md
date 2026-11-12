# shairport-metadata (Rust)

Drop-in replacement for `shairport-metadata.py`: reads the shairport-sync
metadata pipe, sets the zigbee2mqtt lights to the artwork's dominant colour
(only between sunset and sunrise) and sends a 32x32 RGB565 frame to the matrix.
It can also follow a Bose soundbar, so the same happens for music sent straight
to the soundbar (see [Bose soundbar](#bose-soundbar)).

Differences from the Python version:

- One persistent MQTT connection instead of connect/disconnect per command.
- Light states are cached from the subscription, so snapshotting at playback
  start is instant instead of waiting up to 2.5s on `/get` replies.
- No numpy/opencv/sklearn; single ~2MB binary.

## Build

    cargo build --release
    cargo test

## Configuration

Same env as the Python version (`/etc/shairport-metadata.env`, then
`/opt/shairport-metadata/.env`):

| Var | |
|---|---|
| `coordinatorURL`, `coordinatorPort`, `coordinatorUsername`, `coordinatorPassword` | MQTT broker |
| `HA_URL`, `HA_TOKEN` | Home Assistant for sunrise/sunset (cached in `sun_times.json`) |
| `SSNC_PIPE_PATH` | optional, default `/tmp/shairport-sync-metadata` |
| `MATRIX_HOST` | optional, default `matrix.lan` |

## Switch over

The pipe has a single reader, so only one of the two services can run.
The unit declares `Conflicts=shairport-metadata.service`.

    sudo cp shairport-metadata-rs.service /etc/systemd/system/
    sudo systemctl daemon-reload
    sudo systemctl disable --now shairport-metadata.service
    sudo systemctl enable --now shairport-metadata-rs.service

Roll back:

    sudo systemctl disable --now shairport-metadata-rs.service
    sudo systemctl enable --now shairport-metadata.service

## Deploy

Merging to `main` runs `cargo test`, then `.github/workflows/deploy.yml` SSHes to pi5 with the shared
`ci-deploy` key, which can only run `/usr/local/lib/deploy.d/shairport-metadata`. That handler builds the
commit in `/opt/shairport-metadata` (only if it is already on `origin/main`), restarts
`shairport-metadata-rs.service`, and restores the previous binary if the service fails to start.

## Spectrum bars

The matrix's now-playing view shows 7 bars driven by the music. Pieces:

- `audio-tap` (src/bin): started by ALSA's `file` plugin with the stream's rate,
  channels and format; forwards the PCM to `127.0.0.1:5555` over UDP. It never
  blocks, so playback is unaffected if the backend is down.
- `audio.rs`: FFT of the last 2048 samples ~30 times a second, 7 log-spaced bands
  (50 Hz to 14 kHz), each scaled against its own recent peak, sent to the board as
  `"AL" 7 l0..l6` on UDP 9091. Silent for over a second: it stops sending.
- `setup-audio-tap.sh`: one-time Pi setup that routes shairport-sync through the
  tap (`ssh pi5 'bash -s' < rust/setup-audio-tap.sh`, `--undo` to revert).

| Var | |
|---|---|
| `AUDIO_TAP_PORT` | optional, default `5555` |
| `AUDIO_DELAY_MS` | optional, default `150`; delays the bars to line up with what you hear |

## Bose soundbar

With `BOSE_HOST` set, the matrix artwork and light colour also follow what a Bose
soundbar is playing (Bose Music / "ECO2" products: Smart Soundbars, Lifestyle Ultra),
for music sent straight to it: AirPlay to the soundbar, Spotify Connect, and so on.
AirPlay to the Pi wins when both are playing, since it also drives the spectrum bars.
If the Pi's stream ends while the soundbar keeps playing, the soundbar takes over.

How it works (`bose.rs`): the soundbar has a local WebSocket API on port 8082 (TLS
with a self-signed device cert, subprotocol `eco2`), the same one the Bose app uses.
The backend subscribes to `/content/nowPlaying` and the soundbar pushes every change.
Bursts (a skip sends several half-empty states) are settled before acting, and a pause
only ends the session after 4s. AirPlay covers come from the soundbar over HTTP.

The API needs a JWT from your Bose account. It lasts 8 hours; `bose_auth.rs` renews it
with the saved refresh token (Azure B2C, then Bose's id service) and rewrites the token
file, so the password is only needed once:

    # anywhere with Python; writes the token file
    python3 -m venv /tmp/bose-venv && /tmp/bose-venv/bin/pip install pybose requests PyJWT zeroconf
    /tmp/bose-venv/bin/python rust/bose-login.py /tmp/bose_token.json
    scp /tmp/bose_token.json pi5:/opt/shairport-metadata/bose_token.json   # must stay writable by the service user

    # /etc/shairport-metadata.env
    BOSE_HOST=bose-ls-ultra-soundbar.lan   # any name or IP for it; find it with: dns-sd -B _bose-passport._tcp

If the log says the refresh was rejected (password change, revoked sessions), run
`bose-login.py` again. Everything else keeps working meanwhile.

| Var | |
|---|---|
| `BOSE_HOST` | optional; soundbar IP or hostname. Unset: feature off |
| `BOSE_TOKEN_PATH` | optional, default `/opt/shairport-metadata/bose_token.json` |

Checking it by hand:

    BOSE_HOST=... ./shairport-metadata bose-watch     # print what the soundbar reports
    ./shairport-metadata bose-refresh                 # refresh the token now
