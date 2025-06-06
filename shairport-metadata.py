import re, sys, threading, credentials, requests, base64, json, cv2
from sklearn.cluster import KMeans
import paho.mqtt.client as mqtt
from PIL import Image, ImageEnhance
from time import sleep
import numpy as np

DEBUG = False
FOUND_RGB_COLOR=False
MATRIX_WIDTH = 32
MATRIX_HEIGHT = 32

SYNC_KITCHEN=False

# MQTT Broker Info
MQTT_BROKER = "homeassistant.lan"
MQTT_PORT = 1883

# Optional MQTT Credentials
MQTT_USERNAME = credentials.coordinatorUsername
MQTT_PASSWORD = credentials.coordinatorPassword

def debug(s):
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
def color_variance(rgb):
    """Use coefficient of variance to determine variance between rgb colors """
    colors = list(rgb)
    mean = np.mean(colors)
    std = np.std(colors)
    cv = (std / mean) * 100

    return cv
def dominantColor(np_img):
    """
    Receives a NumPy image array (RGB format).
    Returns the most dominant RGB color using KMeans clustering.
    """
    np_img = np.array(np_img).astype(np.uint8) 
    # Optional: resize inside here if you want to ensure consistent performance
    resized = cv2.resize(np_img, (50, 50), interpolation=cv2.INTER_AREA)

    # Reshape to a list of pixels
    pixel_data = resized.reshape((-1, 3))

    # Apply KMeans clustering
    k = 3  # Number of dominant colors to find
    kmeans = KMeans(n_clusters=k, random_state=0).fit(pixel_data)

    # Get the colors (cluster centers) and their counts
    colors = kmeans.cluster_centers_.astype(int)
    labels = kmeans.labels_
    counts = np.bincount(labels)

    # Sort by frequency
    sorted_idx = np.argsort(counts)[::-1]
    dominant = colors[sorted_idx[0]]
    r,g,b = dominant

    
    debug(f"Dominant color (RGB): ({r}, {g}, {b})")
    debug(f"Top {k} colors: {colors[sorted_idx]}")
    # if color is not black or white and the diff between colors is not alot ie white colors return it 
    if not is_blk_white((r, g, b)) and color_variance((r, g, b)) > 5:
        debug(f"returning dominant color ({r}, {g}, {b})")
        return (r,g,b)
    else: 
        debug(f"Could not determine dominant color")
        return None
def is_blk_white(rgb):
    r, g, b = rgb
    brightness = (r + g + b) / 3
    color_spread = max(r, g, b) - min(r, g, b)
    
    return (brightness < 15) or (brightness > 220 and color_spread < 20)
def floyd_steinberg_dither(img_np, color_depth_bits):
    """Apply Floyd–Steinberg dithering to RGB image with specified per-channel bit depth"""
    height, width, _ = img_np.shape
    out = img_np.astype(np.float32)

    max_val = 2**color_depth_bits - 1

    def quantize(val):
        return round(val * max_val / 255) * (255 / max_val)

    for y in range(height):
        for x in range(width):
            old_pixel = out[y, x].copy()
            new_pixel = np.array([quantize(c) for c in old_pixel])
            out[y, x] = new_pixel
            quant_error = old_pixel - new_pixel

            if x + 1 < width:
                out[y, x + 1] += quant_error * 7 / 16
            if y + 1 < height:
                if x > 0:
                    out[y + 1, x - 1] += quant_error * 3 / 16
                out[y + 1, x] += quant_error * 5 / 16
                if x + 1 < width:
                    out[y + 1, x + 1] += quant_error * 1 / 16

    return np.clip(out, 0, 255).astype(np.uint8)
def playbar_off():
    return { "state": "OFF" }
def send_command(topic, payload):
    client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)
    client.username_pw_set(MQTT_USERNAME, MQTT_PASSWORD)
    client.connect(MQTT_BROKER, MQTT_PORT, 60)
    client.loop_start()
    client.publish(topic, json.dumps(payload))
    sleep(0.5)  # Give it time to publish
    client.loop_stop()
    client.disconnect()

def publish_commands(topic_payload_map):
    threads = []
    for topic, payload in topic_payload_map.items():
        t = threading.Thread(target=send_command, args=(topic, payload))
        t.start()
        threads.append(t)
    for t in threads:
        t.join()
def set_light_color(rgb=(255, 255,255), brightness=255):
    return {
        "state": "ON",
        "brightness": brightness,
        "color": {
            "r": int(rgb[0]),
            "g": int(rgb[1]),
            "b": int(rgb[2])
        }
    }
     
def save_and_send_image(name):
    url = "http://matrix.lan/icon.bmp"
    color = None
    playbar_color = None
    global FOUND_RGB_COLOR
    img = Image.open(name).resize((MATRIX_WIDTH,MATRIX_HEIGHT), Image.LANCZOS).convert('RGB')
    
    primaryColor = dominantColor(img)

    # reduce image brightness and saturate it first using pillow library
    img = ImageEnhance.Color(img).enhance(1.5)
    img = ImageEnhance.Brightness(img).enhance(0.4)
    img = ImageEnhance.Contrast(img).enhance(1.5)

    # conver to numpy array for faster processing
    np_img = np.array(img)

    

    if not primaryColor:
        found = False
        # search for non black and white colors
        for y in range(MATRIX_WIDTH):
            for x in range(MATRIX_HEIGHT):
                r,g,b = np_img[y,x]
                #debug(f"Checking pixel ({x},{y}): ({r},{g},{b})")
                if not is_blk_white((r,g,b)):
                    #debug(f"Found non-B/W pixel: ({r},{g},{b}) at position ({x},{y})")
                    cv = color_variance((r,g,b))
                    debug(f"Color variance: {cv}")
                    # if the variance between colors is too low keep searching since colors are similar and likely to produce white color
                    if cv < 5:
                        FOUND_RGB_COLOR=False
                        continue
                    playbar_color = (r,g,b)
                    color = f'#{r:02x}{g:02x}{b:02x}'
                    found = True
                    FOUND_RGB_COLOR=True
                    break
            if found:
                break
    else:
        playbar_color = primaryColor
        r,g,b = primaryColor
        FOUND_RGB_COLOR = True
        color = f'#{r:02x}{g:02x}{b:02x}'



    
    #np_img = ordered_dither(np_img, bit_depth=8)  # or 4 if using 4-bit
    np_img = floyd_steinberg_dither(np_img, color_depth_bits=5)  # or 4

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

    if color:
        set_rgb_color(color)

        topic_payloads = {
                    "zigbee2mqtt/playbar1/set": set_light_color(rgb=playbar_color),  
        "zigbee2mqtt/playbar2/set": set_light_color(rgb=playbar_color)    
        }
        publish_commands(topic_payloads)
        if SYNC_KITCHEN:
            topic_payloads = {
            "zigbee2mqtt/kitchenIsland1/set": set_light_color(rgb=playbar_color),  
            "zigbee2mqtt/kitchenIsland2/set": set_light_color(rgb=playbar_color)    
        }
        publish_commands(topic_payloads)

    else:
        debug("Unable to determine color, setting to default state (off)")
        enable_rgb(False)
        topic_payloads = {
        "zigbee2mqtt/playbar1/set": playbar_off(),
        "zigbee2mqtt/playbar2/set": playbar_off()   
    }
        publish_commands(topic_payloads)
        if SYNC_KITCHEN:
            topic_payloads = {
            "zigbee2mqtt/kitchenIsland1/set": playbar_off(),  
            "zigbee2mqtt/kitchenIsland2/set": playbar_off()  
        }
        publish_commands(topic_payloads)

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

    if not enable:
        topic_payloads = {
        "zigbee2mqtt/playbar1/set": playbar_off(),
        "zigbee2mqtt/playbar2/set": playbar_off()   
    }
        publish_commands(topic_payloads)
        if SYNC_KITCHEN:
            topic_payloads = {
            "zigbee2mqtt/kitchenIsland1/set": playbar_off(),  
            "zigbee2mqtt/kitchenIsland2/set": playbar_off()  
        }
        publish_commands(topic_payloads)

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
            enable_rgb(False)
        # play stream end
        if (typ == "ssnc" and code == "pend"):
            print("\t\tPlay stream end")
            metadata = {}
            print(json.dumps({}))
            sys.stdout.flush()
            clear_artwork()
            enable_rgb(False)
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
                if FOUND_RGB_COLOR:
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
    