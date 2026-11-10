#!/usr/bin/env bash
# Routes shairport-sync's output through the audio tap so the backend can draw
# spectrum bars, then on to the Audioengine as before. Run on pi5 with sudo rights:
#
#   ssh pi5 'bash -s' < rust/setup-audio-tap.sh          # install
#   ssh pi5 'bash -s -- --undo' < rust/setup-audio-tap.sh  # restore hw:CARD=A2
#
# The tap binary comes from the dev build (~/dev/shairport-metadata) if present,
# otherwise from the CI build in /opt/shairport-metadata.
set -euo pipefail

CONF=/etc/shairport-sync.conf
ASOUND=/etc/asound.conf
TAP=/usr/local/bin/shairport-audio-tap
DEVICE=hw:CARD=A2

restart_and_check() {
  sudo systemctl restart shairport-sync
  sleep 4
  systemctl is-active --quiet shairport-sync
}

if [ "${1:-}" = "--undo" ]; then
  sudo sed -i "s|^\(\s*output_device = \)\"shairport_tap\";|\1\"$DEVICE\";|" "$CONF"
  restart_and_check && echo "shairport-sync is back on $DEVICE"
  exit 0
fi

src=""
for f in "$HOME/dev/shairport-metadata/rust/target/release/audio-tap" \
         /opt/shairport-metadata/rust/target/release/audio-tap; do
  [ -x "$f" ] && { src="$f"; break; }
done
[ -n "$src" ] || { echo "no audio-tap build found; run ./dev.sh server first" >&2; exit 1; }

ts=$(date +%Y%m%d-%H%M%S)
sudo install -m 755 "$src" "$TAP"
sudo cp -p "$CONF" "$CONF.bak-$ts"
[ -f "$ASOUND" ] && sudo cp -p "$ASOUND" "$ASOUND.bak-$ts"

sudo tee "$ASOUND" >/dev/null <<EOF
# Copies everything shairport-sync plays to the matrix backend for the spectrum bars,
# then plays it on the Audioengine as before. The helper never blocks: if the backend
# isn't running, the copy is dropped. Undo: rust/setup-audio-tap.sh --undo
pcm.shairport_tap {
    type file
    slave.pcm "$DEVICE"
    file "| exec $TAP %r %c %f"
    format "raw"
}
EOF
sudo sed -i "s|^\(\s*output_device = \)\"$DEVICE\";|\1\"shairport_tap\";|" "$CONF"
grep -q 'output_device = "shairport_tap"' "$CONF" || { echo "couldn't find output_device = \"$DEVICE\" in $CONF" >&2; exit 1; }

if restart_and_check; then
  echo "shairport-sync is playing through the tap (backups: *.bak-$ts)"
else
  echo "shairport-sync didn't start; restoring the backups" >&2
  journalctl -u shairport-sync -n 15 --no-pager -o cat >&2 || true
  sudo cp -p "$CONF.bak-$ts" "$CONF"
  [ -f "$ASOUND.bak-$ts" ] && sudo cp -p "$ASOUND.bak-$ts" "$ASOUND"
  restart_and_check
  exit 1
fi
