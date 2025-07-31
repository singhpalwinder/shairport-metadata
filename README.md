# 🎵 Shairport Metadata to Matrix Dashboard Broadcaster

This Python 3 project runs on a Raspberry Pi Zero 2W and listens to [Shairport-Sync](https://github.com/mikebrady/shairport-sync) metadata output in real-time to extract and decode album artwork during AirPlay streaming. It then processes the image and sends it directly to a C++-based LED matrix dashboard running on an [Adafruit Matrix Portal S3](https://www.adafruit.com/product/5800).

> 🚀 This project **broadcasts artwork to**:  
> 👉 [`matrix-dashboard`](https://github.com/singhpalwinder/matrix-dashboard) — a C++ LED dashboard running on the Matrix Portal S3

---

## 📸 What It Does

- **Listens to AirPlay metadata** using `shairport-sync` via a Unix named pipe.
- **Processes album artwork**:
  - Decodes base64-encoded image data.
  - Resizes image to 32×32 pixels.
  - Converts it to RGB565 byte stream.
- **Sends artwork** to the Matrix Dashboard running C++ on the Matrix Portal S3 via TCP on port `9090`.
- **Extracts dominant colors** using K-Means clustering and adjusts Zigbee-based smart lighting (via Home Assistant) to match the media's vibe.

<p align="center">
  <img src="assets/matrix.JPG" alt="LED Matrix Artwork" width="256"/><br>
  <i>Live AirPlay artwork displayed on Matrix Portal S3 with smart light syncing</i>
</p>

---

## 🧠 How It Works

1. `shairport-sync` writes now-playing metadata to `/tmp/shairport-sync-metadata`.
2. This script parses the stream, looking for:
   - `core/asal` → album name
   - `ssnc/PICT` → embedded album artwork
   - `ssnc/pbeg`/`prsm`/`prgr` → playback events
3. Once a new image is detected:
   - It's decoded and hashed to detect uniqueness
   - Saved to disk
   - Sent as raw RGB565 byte data over TCP to the Matrix Portal S3 (`matrix.lan:9090`)
4. The Matrix Portal S3 C++ code receives and displays the image instantly.
5. A K-Means color analysis selects the best ambient color for the room.
6. Zigbee2MQTT + Home Assistant is used to update lighting based on dominant color tones.

---

## 📂 Project Structure

```bash
.
├── shairport-metadata.py       # Main script (runs on Pi Zero 2W)
├── utils/
│   ├── imageProcessor.py       # Resizes image and computes top K-Means colors
│   └── controlLights.py        # Sends color command to Home Assistant/MQTT
├── assets/
│   └── matrix.JPG              # Example image of the matrix dashboard