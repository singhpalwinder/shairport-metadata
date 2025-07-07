import requests, os

os.rem


class Pironman5:
    def __init__(self, rgb):
        self.rgb=rgb
        self.pironmanURL="http://pi5.lan:34001/api/v1.0/"
    
        self.headers={"Content-Type":"application/json"}
    def rgb_to_hex(self, rgb):
        """Takes in a tuple of r, g,b and returns its hex value"""
        try:
            r,g,b=rgb
            return f'#{r:02x}{g:02x}{b:02x}'
        except TypeError:
            print("invalid color or no color chosen, defaulting to red lights")
            r,g,b=(255,0,0)
            return f'#{r:02x}{g:02x}{b:02x}'
            
    def set_rgb_color(self): 
        color = self.rgb_to_hex(self.rgb)
        res = requests.post(f"{self.pironmanURL}/set-rgb-enable", json={"enable":True}, headers=self.headers)
        response = requests.post(f"{self.pironmanURL}/set-rgb-color", json={"color":color}, headers=self.headers)
        print(f"setting rgb color to: {color}")
        print(f"Pironman5 color change status code: {response.status_code}")
        print(f"Pironman5 enable rgb status code: {res.status_code}")
        #print(response.text)
    def disable_rgb(self):
        url=f"{self.pironmanURL}/set-rgb-enable"
        res = requests.post(url, json={"enable":False})
