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
