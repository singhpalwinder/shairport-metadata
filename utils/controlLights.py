import re
import time
import json
import threading
import paho.mqtt.client as mqtt
import requests
from . import credentials

class ControlLights:
    def __init__(self, rgb):
        self.client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)
        self.rgb = rgb
        self.enable_rgb = False
        self.curr_state = {}   # device -> last known payload from zigbee2mqtt/<device>
        self._snapshots = {}   # device -> snapshot to restore later

        self.mqttConfig = {
            "mqttUsername": credentials.coordinatorUsername,
            "mqttPassword": credentials.coordinatorPassword,
            "mqttURL": credentials.coordinatorURL,
            "mqttPort": credentials.coordinatorPort,
            "topics": [
                "zigbee2mqtt/playbar1/set",
                "zigbee2mqtt/playbar2/set",
                "zigbee2mqtt/Desk/set",
                "zigbee2mqtt/bedLeft/set",
                "zigbee2mqtt/bedRight/set"
            ]
        }

    # ---------- helpers ----------
    def _new_client(self):
        c = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)
        c.username_pw_set(self.mqttConfig["mqttUsername"], self.mqttConfig["mqttPassword"])
        c.connect(self.mqttConfig["mqttURL"], self.mqttConfig["mqttPort"], 60)
        return c

    def _device_name_from_topic(self, topic: str) -> str:
        # "zigbee2mqtt/<name>/set"  -> <name>
        # "zigbee2mqtt/<name>"      -> <name>
        m = re.match(r"^zigbee2mqtt/([^/]+)", topic)
        return m.group(1) if m else topic

    # ---------- state snapshot / restore ----------
    def snapshot_states(self, timeout_sec: float = 2.5):
        """
        Ask each device for its current state and cache it in self._snapshots.
        """
        # map: device -> event for response
        pending = {}
        responses_lock = threading.Lock()

        def on_message(client, userdata, msg):
            try:
                payload = json.loads(msg.payload.decode("utf-8"))
            except Exception:
                return
            device = self._device_name_from_topic(msg.topic)
            # Persist last known state
            with responses_lock:
                self.curr_state[device] = payload
                if device in pending:
                    pending[device].set()

        client = self._new_client()
        client.on_message = on_message

        # Subscribe to all device state topics (without /set)
        devices = [self._device_name_from_topic(t.replace("/set", "")) for t in self.mqttConfig["topics"]]
        for dev in devices:
            client.subscribe(f"zigbee2mqtt/{dev}")

        client.loop_start()

        # Request a fresh report for state/brightness/color
        # Zigbee2MQTT supports /get topic; sending keys with empty strings requests a report.
        for dev in devices:
            pending[dev] = threading.Event()
            client.publish(f"zigbee2mqtt/{dev}/get", json.dumps({
                "state": "",
                "brightness": "",
                "color": "",
                "color_temp": ""
            }))

        # Wait for responses up to timeout
        deadline = time.time() + timeout_sec
        for dev, ev in pending.items():
            remaining = max(0, deadline - time.time())
            ev.wait(remaining)

        client.loop_stop()
        client.disconnect()

        # Build snapshots (only for those we actually got)
        snaps = {}
        for dev in devices:
            if dev in self.curr_state:
                p = self.curr_state[dev]
                snap = {
                    "state": p.get("state", "OFF"),
                }
                # Preserve one of color or color_temp + brightness if present
                if "brightness" in p:
                    snap["brightness"] = p["brightness"]

                # Prefer RGB/HS/XY if present
                if "color" in p and isinstance(p["color"], dict) and p["color"]:
                    snap["color"] = p["color"]
                    if "color_mode" in p:
                        snap["color_mode"] = p["color_mode"]
                elif "color_temp" in p:
                    snap["color_temp"] = p["color_temp"]

                snaps[dev] = snap

        self._snapshots = snaps
        return snaps

    def restore_states(self):
        """
        Push the saved snapshot values back to devices.
        """
        if not self._snapshots:
            print("No snapshots to restore.")
            return

        client = self._new_client()
        client.loop_start()
        infos = []
        for set_topic in self.mqttConfig["topics"]:
            dev = self._device_name_from_topic(set_topic)
            payload = self._snapshots.get(dev)
            if not payload:
                continue
            # If device was OFF, only send OFF (don’t change brightness/color)
            if str(payload.get("state", "OFF")).upper() == "OFF":
                infos.append(client.publish(set_topic, json.dumps({"state": "OFF"})))
            else:
                # If it was ON, send state + brightness + either color or color_temp
                to_send = {"state": "ON"}
                if "brightness" in payload:
                    to_send["brightness"] = payload["brightness"]
                if "color" in payload:
                    to_send["color"] = payload["color"]
                elif "color_temp" in payload:
                    to_send["color_temp"] = payload["color_temp"]
                infos.append(client.publish(set_topic, json.dumps(to_send)))

        for info in infos:
            info.wait_for_publish()
        client.loop_stop()
        client.disconnect()

    def send_command(self, topic, payload):
        client = self._new_client()
        client.loop_start()
        info = client.publish(topic, json.dumps(payload))
        info.wait_for_publish()
        time.sleep(0.2)
        client.loop_stop()
        client.disconnect()

    def publish_commands(self):
        payload = self.format_rgb_phillips_hue(self.rgb)
        print(f"setting playbars to: {payload}")
        client = self._new_client()
        client.loop_start()
        infos = [client.publish(t, json.dumps(payload)) for t in self.mqttConfig["topics"]]
        for info in infos:
            info.wait_for_publish()
        client.loop_stop()
        client.disconnect()

    def format_rgb_phillips_hue(self, rgb, brightness=255):
        try:
            return {
                "state": "ON" if self.enable_rgb else "OFF",
                "brightness": brightness,
                "color": {"r": int(rgb[0]), "g": int(rgb[1]), "b": int(rgb[2])}
            }
        except Exception:
            return {
                "state": "OFF",
                "brightness": brightness,
                "color": {"r": 255, "g": 0, "b": 0}
            }