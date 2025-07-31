# 🎵 Shairport Metadata Listener + LED Matrix Artwork Broadcaster

This Python 3 script runs on a Raspberry Pi Zero 2W and integrates with [`shairport-sync`](https://github.com/mikebrady/shairport-sync) to extract, decode, and act on real-time media metadata. It enhances album artwork, displays it on an LED matrix, and changes Zigbee lights to match the music's color palette — all in real time.

> 🔗 **Based on:** [surekap/MMM-ShairportMetadata](https://github.com/surekap/MMM-ShairportMetadata/blob/master/shairport-metadata.py)

---

## 📸 What It Does

- **Extracts** base64-encoded album artwork and metadata from Shairport-Sync via a named pipe.
- **Processes** the image:
  - Resizes it to 32x32.
  - Converts it to RGB565.
  - Sends it to a remote LED Matrix (64x32) over TCP.
- **Enhances ambience**:
  - Uses K-Means clustering to extract dominant colors.
  - Picks a color with the most variance for optimal lighting mood.
  - Controls Zigbee lights via [Zigbee2MQTT](https://www.zigbee2mqtt.io/) integrated with Home Assistant.
- **Handles edge cases** like monochrome images or missing metadata gracefully by defaulting to green.

<p align="center">
  <img src="assets/matrix.JPG" alt="LED Matrix Artwork" width="256"/><br>
  <i>Live artwork from AirPlay media on an LED matrix + room lights synced to the music's vibe</i>
</p>

---

## 🧠 How It Works

1. Listens to `/tmp/shairport-sync-metadata` for incoming XML metadata.
2. Extracts `core` (album), `ssnc` (playback state), and `PICT` (album artwork).
3. Detects new artwork via hashing.
4. Saves the image to disk and sends it to:
   - The matrix display at `matrix.lan:9090`
   - A light control routine that sets Zigbee lighting color using `controlLights.py`

---

## 📂 Project Structure

```bash
.
├── shairport_listener.py       # Main script (your current file)
├── utils/
│   ├── imageProcessor.py       # Enhances image & extracts dominant color using KMeans
│   └── controlLights.py        # Controls Home Assistant Zigbee lights via MQTT
├── assets/
│   └── matrix.JPG              # Example matrix output photo