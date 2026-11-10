# shairport-metadata (Rust)

Drop-in replacement for `shairport-metadata.py`: reads the shairport-sync
metadata pipe, sets the zigbee2mqtt lights to the artwork's dominant colour
(only between sunset and sunrise) and sends a 32x32 RGB565 frame to the matrix.

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
