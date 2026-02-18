import re, sys, requests, base64, json, cv2, os
from utils.imageProcessor import ImageProcessor
from utils.controlLights import ControlLights
import numpy as np
import socket
from PIL import Image, ImageSequence, ImageEnhance

SSNC_PIPE_PATH="/tmp/shairport-sync-metadata"
DEBUG = True
LAST_SENT=""

# regex to capture shairport-metadata output
REGEX_LINE_ITEM = r"<item><type>(([A-Fa-f0-9]{2}){4})</type><code>(([A-Fa-f0-9]{2}){4})</code><length>(\d*)</length>"
def debug(s):
    if DEBUG:
        print(s)
def prepare_frames(gif_path):
    gif_data = []
    saturation_boost=1.8
    with Image.open(gif_path) as im:
        for frame in ImageSequence.Iterator(im):
            print(f"GIF has {im.n_frames} frames.") 

            # Ensure RGB mode
            frame = frame.convert("RGBA")
            # Resize or crop to 32x32 if needed
            frame = frame.resize((32, 32), Image.LANCZOS)

            # ✅ Composite on black background to preserve transparency
            bg = Image.new("RGBA", frame.size, (0, 0, 0, 255))
            frame = Image.alpha_composite(bg, frame).convert("RGB")

            enhancer = ImageEnhance.Color(frame)
            frame = enhancer.enhance(saturation_boost)

            np_img = np.array(frame)

            # Convert to RGB565
            r = np_img[:, :, 0].astype(np.uint16) & 0xF8
            g = np_img[:, :, 1].astype(np.uint16) & 0xFC
            b = np_img[:, :, 2].astype(np.uint16) >> 3

            rgb565 = (r << 8) | (g << 3) | b
            rgb565_be = rgb565.astype('>H')  # Big-endian
            gif_data.append(rgb565_be.flatten().tobytes())

    return gif_data
def send_test_gif(gif_path):
    HOST = "matrix.lan"
    PORT = 9090

    frames_bytes = prepare_frames(gif_path)

    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.connect((HOST, PORT))
        for idx, frame in enumerate(frames_bytes):
            print(f"Sending frame {idx + 1}/{len(frames_bytes)} ({len(frame)} bytes)")
            s.sendall(frame)

    print("GIF send complete ✅")
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

    zigbee = ControlLights(rgb=primaryColor)
    zigbee.set_lights()

def clear_artwork():
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
    imageData=None
    imageExtension=None
    albumName=None
    metadata = {}

    if not os.path.exists(SSNC_PIPE_PATH):
        raise FileNotFoundError(f"{SSNC_PIPE_PATH} does not exist")
    
    with open(SSNC_PIPE_PATH, 'r') as pipe:
        while True:
            
            line = pipe.readline()
            if not line:    #EOF
                break
            sys.stdout.flush()
            if not line.startswith("<item>"):
                continue
            typ, code, length = start_item(line)

            data = ""
            if (length > 0):
                r = start_data(pipe.readline())
                if (r == -1):
                    continue
                data = read_data(pipe.readline(), length)

            # Everything read
            if (typ == "core"):
                # album name
                if (code == "asal"): 
                    try:
                        metadata['Album Name'] = data.decode()
                        albumName=data.decode()
                    except:
                        metadata["Album Name"] = "Unkown"
                elif (code == "asar"):
                    metadata['Artist'] = data.decode()
                #elif (code == "ascm"):
                #    metadata['Comment'] = data
                #elif (code == "asgn"):
                #    metadata['Genre'] = data
                elif (code == "minm"):
                    metadata['Title'] = data.decode()
                #elif (code == "ascp"):
                #    metadata['Composer'] = data
                #elif (code == "asdt"):
                #    metadata['File Kind'] = data
                #elif (code == "assn"):
                #    metadata['Sort as'] = data
                #elif (code == "clip"):
                #    metadata['IP'] = data
            if (typ == "ssnc" and code == "snam"):
                metadata['snam'] = data.decode()
            if (typ == "ssnc" and code == "prgr"):
                metadata['prgr'] = data.decode()
                # play stream flush
            if (typ == "ssnc" and code == "pfls"):
                print("\t\tPlay stream flush")
                metadata = {}
                print(json.dumps({}))
                sys.stdout.flush()
                clear_artwork()
                delete_artwork()
            # play stream end
            if (typ == "ssnc" and code == "pend"):
                print("\t\tPlay stream end")
                metadata = {}
                print(json.dumps({}))
                sys.stdout.flush()
                delete_artwork()
                clear_artwork()
            # play stream resume
            if (typ == "ssnc" and code == "prsm"):
                metadata['pause'] = False
            # play stream begin
            if (typ == "ssnc" and code == "pbeg"):
                metadata['pause'] = False
            if (typ == "ssnc" and code == "PICT"):
                if (len(data) == 0):
                    # clear_artwork()
                    # enable_rgb(False)
                    print(json.dumps({"image": ""}))
                    continue
                else:
                    mime = guessImageMime(data)
                    extension = {
                        'image/jpeg': '.jpg',
                        'image/png': '.png'
                    }.get(mime, '.jpg')  # Default to .jpg
                    imageData = data
                    imageExtension=extension
                    print(json.dumps({"image": "data:" + mime}))
                sys.stdout.flush()
            # track changed
            if (typ == "ssnc" and code == "mden"):
                print(json.dumps(metadata))
                sys.stdout.flush()
                metadata = {}
                #clear_artwork()
                delete_artwork()
            
            if albumName and imageData:
                fileName = f"{albumName.lower().replace(' ', '_')}{imageExtension}"
                
                if albumName == LAST_SENT:
                    print(f"{albumName} already sent")
                    continue
                
                delete_artwork()
                
                print(f"Writing image {fileName} to disk....")
                with open(fileName, "wb") as img_file:
                    img_file.write(imageData)
                    img_file.flush()
                os.sync()

                print(f"Sending {fileName}...")
                #save_and_send_image(fileName)
                send_test_gif("kobe.gif")

                LAST_SENT = albumName
                imageData = None
                imageExtension = None
                albumName=None
                
                