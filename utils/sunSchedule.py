import os
import json
import requests
from datetime import datetime, timedelta, time as dtime
from dotenv import load_dotenv

load_dotenv("/opt/shairport-metadata/.env")

CACHE_PATH = "/opt/shairport-metadata/sun_times.json"
MAX_AGE = timedelta(hours=12)


class SunSchedule:
    """
    Decides whether it's dark enough to drive the lights.

    Sunrise/sunset come from Home Assistant's sun.sun entity and are cached on
    disk as local time-of-day plus a last_checked timestamp. HA is only queried
    when the cache is older than MAX_AGE; if HA is unreachable the cached times
    are used as-is.
    """

    def __init__(self, cache_path=CACHE_PATH):
        self.cache_path = cache_path
        self.ha_url = os.environ.get("HA_URL", "http://homeassistant.lan:8123")
        self.ha_token = os.environ.get("HA_TOKEN")
        self.cache = self._load_cache()

    # ---------- cache ----------
    def _load_cache(self):
        try:
            with open(self.cache_path) as f:
                c = json.load(f)
            return {
                "sunrise": dtime.fromisoformat(c["sunrise"]),
                "sunset": dtime.fromisoformat(c["sunset"]),
                "last_checked": datetime.fromisoformat(c["last_checked"]),
            }
        except (OSError, ValueError, KeyError) as e:
            print(f"No usable sun cache at {self.cache_path}: {e}")
            return None

    def _save_cache(self):
        tmp = self.cache_path + ".tmp"
        with open(tmp, "w") as f:
            json.dump({
                "sunrise": self.cache["sunrise"].isoformat(timespec="seconds"),
                "sunset": self.cache["sunset"].isoformat(timespec="seconds"),
                "last_checked": self.cache["last_checked"].isoformat(timespec="seconds"),
            }, f, indent=2)
        os.replace(tmp, self.cache_path)

    # ---------- Home Assistant ----------
    def refresh(self):
        """Fetch sunrise/sunset from HA and update the cache. Returns True on success."""
        if not self.ha_token:
            print("HA_TOKEN not set; using cached sun times")
            return False
        try:
            res = requests.get(
                f"{self.ha_url}/api/states/sun.sun",
                headers={"Authorization": f"Bearer {self.ha_token}"},
                timeout=5,
            )
            res.raise_for_status()
            attrs = res.json()["attributes"]
            # next_* are UTC timestamps; keep only the local time-of-day
            sunrise = datetime.fromisoformat(attrs["next_rising"]).astimezone().time()
            sunset = datetime.fromisoformat(attrs["next_setting"]).astimezone().time()
        except (requests.RequestException, ValueError, KeyError) as e:
            print(f"Could not reach Home Assistant for sun times: {e}")
            return False

        self.cache = {
            "sunrise": sunrise.replace(microsecond=0),
            "sunset": sunset.replace(microsecond=0),
            "last_checked": datetime.now().astimezone().replace(microsecond=0),
        }
        try:
            self._save_cache()
        except OSError as e:
            print(f"Failed to write sun cache: {e}")
        print(f"🌅 Sun times updated: sunrise {self.cache['sunrise']}, sunset {self.cache['sunset']}")
        return True

    # ---------- decision ----------
    def is_dark(self):
        now = datetime.now().astimezone()
        if self.cache is None or now - self.cache["last_checked"] > MAX_AGE:
            self.refresh()

        if self.cache is None:
            # never fetched and HA unreachable: don't block the lights
            print("No sun times available; allowing lights")
            return True

        t = now.time()
        return t >= self.cache["sunset"] or t < self.cache["sunrise"]
