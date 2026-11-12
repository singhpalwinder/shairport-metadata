#!/usr/bin/env python3
"""One-time Bose account login for the soundbar now-playing feature.

Logs in the way the Bose app does (via pybose) and writes the token file the Rust
backend uses. After this the backend refreshes the token by itself; run this again
only if the log says the refresh was rejected (password change, revoked session).

    python3 -m venv /tmp/bose-venv
    /tmp/bose-venv/bin/pip install pybose requests PyJWT zeroconf
    /tmp/bose-venv/bin/python bose-login.py [/opt/shairport-metadata/bose_token.json]

The password is only sent to Bose's login servers and never written to disk.
"""
import getpass
import json
import os
import sys

from pybose.BoseAuth import BoseAuth

out = sys.argv[1] if len(sys.argv) > 1 else "/opt/shairport-metadata/bose_token.json"
email = input("Bose account email: ").strip()
password = getpass.getpass("Bose account password (hidden): ")

auth = BoseAuth()
tok = auth.getControlToken(email, password)
data = {
    "access_token": tok["access_token"],
    "bose_refresh_token": tok["refresh_token"],
    "bose_person_id": tok["bose_person_id"],
    "azure_refresh_token": auth.get_azure_refresh_token(),
}
tmp = out + ".tmp"
fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as f:
    json.dump(data, f, indent=1)
os.replace(tmp, out)
print(f"OK. Token valid for ~{auth.get_token_validity_time() // 60} min. Saved to {out}")
