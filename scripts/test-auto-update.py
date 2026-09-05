#!/usr/bin/env python3
"""Fixture tests for scripts/auto-update.sh: the idle gate decides, the installer is a stub."""
import http.server, json, os, subprocess, sys, tempfile, threading
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "auto-update.sh"

class StatusServer:
    def __init__(self, payload):
        self.payload = payload
        handler = self._handler()
        self.httpd = http.server.HTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
    def _handler(s):
        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                body = json.dumps(s.payload).encode()
                self.send_response(200); self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
            def log_message(self, *a): pass
        return H
    @property
    def url(self): return f"http://127.0.0.1:{self.httpd.server_port}/agent/status"
    def close(self): self.httpd.shutdown()

def run(status, latest, *args, marker=None, home=None):
    env = dict(os.environ, HOME=home, PHONE_REMOTE_STATE_DIR=str(Path(home) / "state"),
               AUTO_UPDATE_STATUS_URL=status.url, AUTO_UPDATE_TOKEN="t", AUTO_UPDATE_LATEST_TAG=latest,
               AUTO_UPDATE_INSTALLER_CMD=f"touch '{marker}'")
    p = subprocess.run(["bash", str(SCRIPT), "run", *args], env=env, capture_output=True, text=True, timeout=30)
    return p.returncode, p.stderr

base = {"ok": True, "version": "0.5.4", "owner": None, "owner_lease_remaining_secs": 0,
        "hold_remaining_secs": 0, "releasing": False, "reconnecting": False, "device_state": "released"}
failures = 0
def case(name, status_payload, latest, *args, expect_install, expect_decision):
    global failures
    with tempfile.TemporaryDirectory() as home:
        marker = Path(home) / "installed"
        srv = StatusServer(status_payload)
        try:
            rc, err = run(srv, latest, *args, marker=marker, home=home)
        finally:
            srv.close()
        ok = (marker.exists() == expect_install) and (expect_decision in err) and rc == 0
        print(("ok  " if ok else "FAIL"), name, "" if ok else f"rc={rc} installed={marker.exists()} err={err.strip()[-160:]}")
        failures += not ok

case("up to date → skip", base, "v0.5.4", expect_install=False, expect_decision="skip:up_to_date")
case("newer + idle → upgrade", base, "v0.6.0", expect_install=True, expect_decision="upgrade current=0.5.4 latest=0.6.0")
case("newer + owner → skip", {**base, "owner": "bank-flow", "owner_lease_remaining_secs": 120}, "v0.6.0", expect_install=False, expect_decision="skip:phone_owned owner=bank-flow")
case("newer + hold → skip", {**base, "hold_remaining_secs": 300}, "v0.6.0", expect_install=False, expect_decision="skip:held")
case("newer + ready (WDA up) → skip", {**base, "device_state": "ready"}, "v0.6.0", expect_install=False, expect_decision="skip:in_use")
case("newer + reconnecting → skip", {**base, "reconnecting": True}, "v0.6.0", expect_install=False, expect_decision="skip:transitioning")
case("newer + blocked (crash loop) → upgrade", {**base, "device_state": "blocked"}, "v0.6.0", expect_install=True, expect_decision="upgrade")
case("--dry-run never installs", base, "v0.6.0", "--dry-run", expect_install=False, expect_decision="dry-run: would run")
case("--force ignores the owner", {**base, "owner": "bank-flow"}, "v0.6.0", "--force", expect_install=True, expect_decision="upgrade")
case("--force still needs a newer release", base, "v0.5.4", "--force", expect_install=False, expect_decision="skip:up_to_date")
case("--reinstall upgrades even when current", base, "v0.5.4", "--reinstall", expect_install=True, expect_decision="upgrade")
case("older 'latest' (rollback on GitHub) → skip", base, "v0.5.3", expect_install=False, expect_decision="skip:up_to_date")
case("daemon not ok → skip", {**base, "ok": False}, "v0.6.0", expect_install=False, expect_decision="skip:daemon_not_ok")
print("FAILED" if failures else "OK", f"({13 - failures}/13)")
sys.exit(1 if failures else 0)
