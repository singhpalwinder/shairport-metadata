import paho.mqtt.client as mqtt
import threading, json, requests
from time import sleep
from . import credentials

class ControlLights:
    def __init__(self, rgb):
        self.client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)
        self.rgb = rgb
        self.enable_rgb = False
        #self.pi5=Pironman5(rgb=self.rgb)
        self.mqttConfig = {
            "mqttUsername": credentials.coordinatorUsername,
            "mqttPassword": credentials.coordinatorPassword,
            "mqttURL": credentials.coordinatorURL,
            "mqttPort": credentials.coordinatorPort,
            "topics": ["zigbee2mqtt/playbar1/set", "zigbee2mqtt/playbar2/set", "zigbee2mqtt/Desk/set", "zigbee2mqtt/bedLeft/set", "zigbee2mqtt/bedRight/set"]
        }

    def send_command(self, topic, payload):
        config = self.mqttConfig
        client = self.client
        client.username_pw_set(config["mqttUsername"], config["mqttPassword"])
        client.connect(config["mqttURL"], config["mqttPort"], 60)
        client.loop_start()
        info = client.publish(topic, json.dumps(payload))
        info.wait_for_publish()
        sleep(5)
        client.loop_stop()
        client.disconnect()

    def publish_commands(self):
        payload = self.format_rgb_phillips_hue(self.rgb)
        print(f"setting playbars to: {payload}")

        config = self.mqttConfig
        client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)
        client.username_pw_set(config["mqttUsername"], config["mqttPassword"])
        client.connect(config["mqttURL"], config["mqttPort"], 60)
        client.loop_start()

        infos = []
        for topic in config["topics"]:
            infos.append(client.publish(topic, json.dumps(payload)))

        # Wait for all publishes
        for info in infos:
            info.wait_for_publish()

        client.loop_stop()
        client.disconnect()
    def format_rgb_phillips_hue(self, rgb, brightness=255):
        try:
            return {
                "state": "ON" if self.enable_rgb else "OFF",
                "brightness": brightness,
                "color": {"r": rgb[0], "g": rgb[1], "b": rgb[2]}
            }
        except:
            return {
                "state": "OFF",
                "brightness": brightness,
                "color": {"r": 255, "g": 0, "b": 0}
            }

    def get_lux_value(self):
        res = requests.get("http://esp32.lan/lux")
        return res.json()["lux"]

    def set_lights(self):
        curr_lux = self.get_lux_value()

        if curr_lux < 10:
            self.enable_rgb = True
            print(f"Curr lux value: {curr_lux} enabling RGB")
            self.publish_commands()
            #self.pi5.set_rgb_color()
        else:
            self.enable_rgb = False
            print("Disabling RGB")
            self.publish_commands()
            #self.pi5.disable_rgb()
    def disable_lights(self):
        self.enable_rgb = False
        print("Disabling RGB")
        self.publish_commands()
        #self.pi5.disable_rgb()



