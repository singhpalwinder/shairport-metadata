import re, sys
import base64
import json
from PIL import Image, ImageEnhance
import requests, io
from time import sleep
DEBUG = False

def debug(s):
    global DEBUG
    if DEBUG:
        print(s)
def set_rgb_color(color):
    url = "http://localhost:34001/api/v1.0/set-rgb-color"
    headers = {
        "Content-Type":"application/json"
    }
    response = requests.post(url, json={"color":color}, headers=headers)
    debug(f"setting rgb color to: {color}")
    debug(response.status_code)
    debug(response.text)
def is_blk_white(rgb):
    r, g, b = rgb
    return (r <= 10 and g <= 10 and b <= 10) or (r >= 245 and g >= 245 and b >= 245)
def save_and_send_image(ext):
    url = "http://matrix.lan/icon.bmp"
    color = None
    img = Image.open(f"curr-song{ext}").resize((32,32), Image.LANCZOS).convert('RGB')

        # search for non black and white colors
    for y in range(32):
        for x in range(32):
            r,g,b = img.getpixel((x,y))
            debug(f"Checking pixel ({x},{y}): ({r},{g},{b})")
            if not is_blk_white((r,g,b)):
                debug(f"Found non-B/W pixel: ({r},{g},{b})")
                sleep(3)
                color = f'#{r:02x}{g:02x}{b:02x}'
                break
        else:
            continue
        break

    enhancer = ImageEnhance.Brightness(img)
    img = enhancer.enhance(0.5)
    img_bytes = img.tobytes()

    debug(f"Image size: {len(img_bytes)} bytes")
    response = requests.post(url, data=img_bytes)
    if color:
        set_rgb_color(color)
    else:
        debug("Unable to determine color, setting to default color: #FF0000 (red)")
        set_rgb_color("#FF0000")


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
def enable_rgb(enable=False):
    url = "http://localhost:34001/api/v1.0/set-rgb-enable"
    headers = {
        "Content-Type":"application/json"
    }
    response = requests.post(url, json={"enable":enable}, headers=headers)
    print("Enabling rgb..." if enable else "Disabling rgb...")
    debug(response.status_code)
    debug(response.text)
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
            if (code == "asal"):
                metadata['Album Name'] = data.decode()
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
        if (typ == "ssnc" and code == "pfls"):
            metadata = {}
            print(json.dumps({}))
            sys.stdout.flush()
            # clear_artwork()
            # enable_rgb(False)
        if (typ == "ssnc" and code == "pend"):
            metadata = {}
            print(json.dumps({}))
            sys.stdout.flush()
            clear_artwork()
            enable_rgb(False)
        if (typ == "ssnc" and code == "prsm"):
            metadata['pause'] = False
        if (typ == "ssnc" and code == "pbeg"):
            metadata['pause'] = False
        if (typ == "ssnc" and code == "PICT"):
            if (len(data) == 0):
                clear_artwork()
                enable_rgb(False)
                print(json.dumps({"image": ""}))
            else:
                mime = guessImageMime(data)
                extension = {
                    'image/jpeg': '.jpg',
                    'image/png': '.png'
                }.get(mime, '.jpg')  # Default to .jpg
                if DEBUG:
                    filename = "curr-song" + extension
                    with open(filename, "wb") as img_file:
                        img_file.write(data)
                save_and_send_image(extension)
                enable_rgb(True)
                # to print the base 64 code along with the image type print (json.dumps({"image": "data:" + mime + ";base64," + base64.b64encode(data).decode()}))
                print (json.dumps({"image": "data:" + mime}))
                
            sys.stdout.flush()
        # track changed
        if (typ == "ssnc" and code == "mden"):
            print(json.dumps(metadata))
            sys.stdout.flush()
            metadata = {}
            #clear_artwork()
    