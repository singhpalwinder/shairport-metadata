import re, sys, requests, base64, json, cv2, os, socket, hashlib
from utils.imageProcessor import ImageProcessor
from utils.controlLights import ControlLights
import numpy as np

SSNC_PIPE_PATH="/tmp/shairport-sync-metadata"
DEBUG = True
LAST_SENT=""

# --- lights controller  ---
lights = ControlLights(rgb=(0, 0, 0))
_have_snapshot = False

# regex to capture shairport-metadata output
REGEX_LINE_ITEM = r"<item><type>(([A-Fa-f0-9]{2}){4})</type><code>(([A-Fa-f0-9]{2}){4})</code><length>(\d*)</length>"
def debug(s):
    if DEBUG:
        print(s)
     
def save_and_send_image(name):
    print(f"processing and sending image: {name}")
    HOST="matrix.lan"
    PORT=9090
    color = None
    ip = ImageProcessor(name)
    primaryColor = ip.dominant_color()

    np_img = ip.enhance_image()

    # casting numpy array before shifting to prevent overflow errors
    r = np_img[:, :, 0].astype(np.uint16) & 0xF8
    g = np_img[:, :, 1].astype(np.uint16) & 0xFC
    b = np_img[:, :, 2].astype(np.uint16) >> 3
    rgb565 = (r << 8) | (g << 3) | b

    # Flatten to byte array: high byte first, low byte second
    # img_rgb565 = bytearray()
    # for val in rgb565.flatten():
    #     img_rgb565.append((val >> 8) & 0xFF)  # high byte
    #     img_rgb565.append(val & 0xFF)         # low byte

    rgb565_be = rgb565.astype('>H') # >H means big-endian uint16

    # get binary bytes
    img_rgb565 = rgb565_be.flatten().tobytes()

    debug(f"Image size: {len(img_rgb565)} bytes")

    # send image over socket connection
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.connect((HOST,PORT))
        s.sendall(img_rgb565)

    # set lights for the track artwork color
    lights.rgb = tuple(int(x) for x in primaryColor)
    lights.enable_rgb = True
    lights.publish_commands()

def clear_matrix_artwork():
    try:
        debug("Clearing artwork...")
        res = requests.post("http://matrix.lan/reset")
        debug(res.status_code)
        debug(res.text)
    except requests.exceptions.ConnectionError as e:
        print(f"there was an error sending reset command to matrix: {e}")
        return
def start_item(line):
    matches = re.findall(REGEX_LINE_ITEM, line)
    typ = bytes.fromhex(matches[0][0]).decode('utf-8', errors='ignore')
    code = bytes.fromhex(matches[0][2]).decode('utf-8', errors='ignore')
    length = int(matches[0][4])
    return (typ, code, length)
def start_data(line):
    try:
        assert line == '<data encoding="base64">\n'
    except AssertionError:
        if line.startswith("<data"):
            return 0
        return -1
    return 0

def read_data(line, length):
    b64size = 4*((length+2)//3);
    try:
        data = base64.b64decode(line[:b64size].encode())
    except TypeError:
        data = ""
        pass
    return data

def guessImageMime(magic):

    if magic.startswith(b'\xff\xd8'):
        return 'image/jpeg'
    elif magic.startswith(b'\x89PNG\r\n\x1a\r'):
        return 'image/png'
    else:
        return "image/jpg"
def delete_artwork():
    for file in os.listdir():
        if file.endswith(".jpg") or file.endswith(".png"):
            print(f"Deleting stale artwork {file}...")
            os.remove(file)
if __name__ == "__main__":
    if not os.path.exists(SSNC_PIPE_PATH):
        raise FileNotFoundError(f"{SSNC_PIPE_PATH} does not exist")

    LAST_SENT = ""
    track_state = {
        "album": None,
        "image_data": None,
        "image_extension": None,
        "image_hash": None,
        "ready": False,
        "sent": False
    }

    with open(SSNC_PIPE_PATH, 'r') as pipe:
        while True:
            line = pipe.readline()
            if not line:
                break
            if not line.startswith("<item>"):
                continue

            typ, code, length = start_item(line)
            data = ""
            if length > 0:
                if start_data(pipe.readline()) == -1:
                    continue
                data = read_data(pipe.readline(), length)

            # ========== METADATA ==========

            if typ == "core" and code == "asal":
                try:
                    new_album = data.decode(errors="ignore")
                    if new_album != track_state["album"]:
                        track_state["album"] = new_album
                        track_state["sent"] = False  # new album → allow resend
                except:
                    track_state["album"] = None

            elif typ == "ssnc" and code == "PICT":
                if len(data) == 0:
                    print(json.dumps({"image": ""}))
                    sys.stdout.flush()
                    continue

                mime = guessImageMime(data)
                ext = {
                    'image/jpeg': '.jpg',
                    'image/png': '.png'
                }.get(mime, '.jpg')
                img_hash = hashlib.md5(data).hexdigest()

                if img_hash != track_state.get("image_hash"):
                    track_state["image_data"] = data
                    track_state["image_extension"] = ext
                    track_state["image_hash"] = img_hash
                    track_state["sent"] = False  # image changed → resend
                    print(json.dumps({"image": f"data:{mime}"}))
                    sys.stdout.flush()

            # ====== Playback started/resumed/progressed ======
            elif typ == "ssnc" and code in ["pbeg", "prsm", "prgr"]:
                track_state["ready"] = True
                if not _have_snapshot:
                    print("🎛️ Snapshotting light states...")
                    lights.snapshot_states()   # grabs ON/OFF + brightness + color/ct per device
                    _have_snapshot = True
            # ====== Track ended/flushed/reset ======
            elif typ == "ssnc" and code in ["pend", "pfls"]:

                if _have_snapshot:
                    print("↩️ Restoring light states...")
                    lights.restore_states()
                    _have_snapshot = False

                print(f"🧼 Stream reset: {code}")
                track_state = {
                    "album": None,
                    "image_data": None,
                    "image_extension": None,
                    "image_hash": None,
                    "ready": False,
                    "sent": False
                }
                clear_matrix_artwork()
                delete_artwork()
                print(json.dumps({}))
                sys.stdout.flush()

            # ========== Ready to Send Image? ==========

            if (
                track_state["ready"]
                and not track_state["sent"]
                and track_state["album"]
                and track_state["image_data"]
            ):
                file_name = f"{track_state['album'].lower().replace(' ', '_')}{track_state['image_extension']}"
                delete_artwork()
                print(f"📀 Writing image {file_name} to disk...")
                with open(file_name, "wb") as f:
                    f.write(track_state["image_data"])
                    f.flush()
                os.sync()

                print(f"📤 Sending {file_name}...")
                save_and_send_image(file_name)

                LAST_SENT = track_state["album"]
                track_state["sent"] = True