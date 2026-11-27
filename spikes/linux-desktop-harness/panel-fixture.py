"""Feed a recording state to the native UI when the null audio graph is unstable."""
import hashlib
import json
import os
import socketserver
import time
from datetime import datetime, timezone
from pathlib import Path

profile = Path(os.environ["BROSCRIBE_DATA_DIR"]).resolve()
runtime = Path(os.environ["XDG_RUNTIME_DIR"]) / "broscribe"
library = os.environ["BROSCRIBE_LIBRARY"]
profile.mkdir(parents=True, exist_ok=True)
runtime.mkdir(mode=0o700, exist_ok=True)
Path(library).mkdir(parents=True, exist_ok=True)
(profile / "config.toml").write_text(
    '[app]\nopen_at_login = false\n'
    '[setup]\ncompleted_steps = ["assistant"]\n'
    '[appearance]\nreading_size = "largest"\n'
)
key = hashlib.sha256(os.fsencode(profile)).hexdigest()[:16]
endpoint = runtime / f"broscribed-{key}.sock"
state = {
    "pid": os.getpid(),
    "version": "synthetic",
    "protocol": 8,
    "library": library,
    "jobs": {"running": None, "waiting": 0, "deferred": 0},
    "preview": {"state": "off"},
    "recording": {
        "session_id": "synthetic-panel",
        "status": "recording",
        "title": "Moonberry panel rehearsal",
        "started_at": datetime.now(timezone.utc).isoformat(),
        "elapsed_s": 0.0,
        "health": {"tracks": [
            {"track": "mic", "state": "capturing", "dropped_frames": 0, "failures": 0},
            {"track": "system", "state": "waiting", "dropped_frames": 0, "failures": 1},
        ]},
    },
}


class Protocol(socketserver.StreamRequestHandler):
    def handle(self):
        for line in self.rfile:
            request = json.loads(line)
            method = request["method"]

            def send(**fields):
                message = json.dumps({"v": request["v"], **fields}) + "\n"
                self.wfile.write(message.encode())

            if method == "subscribe":
                send(id=request["id"], ok=True, result={"subscribed": True})
                send(event="state", **state)
                # No state changes after this snapshot.
                time.sleep(180)
                return
            if method == "ping":
                send(id=request["id"], ok=True, result={
                    "pid": os.getpid(), "version": "synthetic", "protocol": 8,
                    "build": None, "mode": "normal", "readiness": "normal_operational",
                })
            elif method == "status":
                send(id=request["id"], ok=True, result=state)
            else:
                send(id=request["id"], ok=False, error={
                    "code": "invalid_request", "message": "Read-only synthetic panel fixture.",
                })


os.umask(0o077)
with socketserver.ThreadingUnixStreamServer(str(endpoint), Protocol) as server:
    server.daemon_threads = True
    server.serve_forever()
