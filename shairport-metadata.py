import re, sys, requests, base64, json, cv2
from sklearn.cluster import KMeans
from utils.imageProcessor import ImageProcessor
from utils.controlLights import ControlLights
import numpy as np

DEBUG = False

def debug(s):
    if DEBUG:
        print(s)
     
def save_and_send_image(name):
    url = "http://matrix.lan/icon.bmp"
    color = None
    ip = ImageProcessor(name)
    primaryColor = ip.dominant_color()

    #np_img = ordered_dither(np_img, bit_depth=8)  # or 4 if using 4-bit
    np_img = ip.enhance_image()

    # casting numpy array before shifting to prevent overflow errors
    r = np_img[:, :, 0].astype(np.uint16) & 0xF8
    g = np_img[:, :, 1].astype(np.uint16) & 0xFC
    b = np_img[:, :, 2].astype(np.uint16) >> 3
    rgb565 = (r << 8) | (g << 3) | b

    # Flatten to byte array: high byte first, low byte second
    img_rgb565 = bytearray()
    for val in rgb565.flatten():
        img_rgb565.append((val >> 8) & 0xFF)  # high byte
        img_rgb565.append(val & 0xFF)         # low byte

    debug(f"Image size: {len(img_rgb565)} bytes")
    response = requests.post(url, data=img_rgb565)

    zigbee = ControlLights(rgb=primaryColor)
    zigbee.set_lights()

    print(f"Status: {response.status_code}")
    debug(f"Response: {response.text}")
def clear_artwork():
    debug("Clearing artwork...")
    res = requests.post("http://matrix.lan/reset")
    debug(res.status_code)
    debug(res.text)
def start_item(line):
    regex = r"<item><type>(([A-Fa-f0-9]{2}){4})</type><code>(([A-Fa-f0-9]{2}){4})</code><length>(\d*)</length>"
    matches = re.findall(regex, line)
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

if __name__ == "__main__":
    metadata = {}
    fi = sys.stdin
    while True:
        line = sys.stdin.readline()
        if not line:    #EOF
            break
        sys.stdout.flush()
        if not line.startswith("<item>"):
            continue
        typ, code, length = start_item(line)

        data = ""
        if (length > 0):
            r = start_data(sys.stdin.readline())
            if (r == -1):
                continue
            data = read_data(sys.stdin.readline(), length)

        # Everything read
        if (typ == "core"):
            # album name
            if (code == "asal"): 
                try:
                    metadata['Album Name'] = data.decode()
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
        # play stream end
        if (typ == "ssnc" and code == "pend"):
            print("\t\tPlay stream end")
            metadata = {}
            print(json.dumps({}))
            sys.stdout.flush()
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
                
                album_name = metadata.get("Album Name", None)
                filename = ""
                if album_name:
                    filename = f"{album_name}{extension}"
                else:
                    filename = f"curr-song{extension}"
                
                with open(filename, "wb") as img_file:
                        img_file.write(data)
                save_and_send_image(filename)
                # to print the base 64 code along with the image type print (json.dumps({"image": "data:" + mime + ";base64," + base64.b64encode(data).decode()}))
                print (json.dumps({"image": "data:" + mime}))
            sys.stdout.flush()
        # track changed
        if (typ == "ssnc" and code == "mden"):
            print(json.dumps(metadata))
            sys.stdout.flush()
            metadata = {}
            #clear_artwork()