"""Feed a recording state to the native UI when the null audio graph is unstable.

It answers on the agent's socket in place of broscribed, so start it instead
of the agent, before the app. `--showcase` serves the README screenshot
state from showcase.py: a paused recording (its elapsed time does not tick)
with a few live transcript lines and the existing config left as it is.
"""
import argparse
import hashlib
import json
import os
import socketserver
import time
from datetime import datetime, timezone
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--showcase", action="store_true")
args = parser.parse_args()

profile = Path(os.environ["BROSCRIBE_DATA_DIR"]).resolve()
runtime = Path(os.environ["XDG_RUNTIME_DIR"]) / "broscribe"
library = os.environ["BROSCRIBE_LIBRARY"]
profile.mkdir(parents=True, exist_ok=True)
runtime.mkdir(mode=0o700, exist_ok=True)
Path(library).mkdir(parents=True, exist_ok=True)
if not args.showcase:
    (profile / "config.toml").write_text(
        '[app]\nopen_at_login = false\n'
        '[setup]\ncompleted_steps = ["assistant"]\n'
        '[appearance]\nreading_size = "largest"\n'
    )
key = hashlib.sha256(os.fsencode(profile)).hexdigest()[:16]
endpoint = runtime / f"broscribed-{key}.sock"
recording = {
    "session_id": "synthetic-panel",
    "status": "recording",
    "title": "Moonberry panel rehearsal",
    "started_at": datetime.now(timezone.utc).isoformat(),
    "elapsed_s": 0.0,
    "health": {"tracks": [
        {"track": "mic", "state": "capturing", "dropped_frames": 0, "failures": 0},
        {"track": "system", "state": "waiting", "dropped_frames": 0, "failures": 1},
    ]},
}
preview = {"state": "off"}
live = []
if args.showcase:
    import showcase

    recording.update(showcase.RECORDING, status="paused", health={"tracks": [
        {"track": "mic", "state": "capturing", "dropped_frames": 0, "failures": 0},
        {"track": "system", "state": "capturing", "dropped_frames": 0, "failures": 0},
    ]})
    preview = {"state": "ready", "model": "parakeet-tdt-0.6b-v3"}
    for start, track, person, text in showcase.LIVE_LINES:
        speaker = {"kind": "me"}
        if person:
            person_id, name = showcase.PEOPLE[person]
            speaker = {"kind": "person", "person_id": person_id, "name": name}
        live.append({
            "event": "live_transcript", "session_id": recording["session_id"],
            "segment": {"track": track, "start_s": float(start),
                        "end_s": float(start) + max(2.5, len(text.split()) * 0.38),
                        "text": text, "speaker": speaker},
        })
state = {
    "pid": os.getpid(),
    "version": "synthetic",
    "protocol": 8,
    "library": library,
    "jobs": {"running": None, "waiting": 0, "deferred": 0},
    "preview": preview,
    "recording": recording,
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
                for event in live:
                    send(**event)
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
            elif method == "record.live":
                # No live speaker analysis: the lines carry their speakers.
                send(id=request["id"], ok=True, result={
                    "session_id": recording["session_id"], "evidence": "synthetic",
                    "identity": {"status": {"state": "idle"}, "epoch": 0}, "turns": [],
                })
            elif method == "queue.kick":
                send(id=request["id"], ok=True, result={"job_ids": []})
            else:
                print(f"refused {method}", flush=True)
                send(id=request["id"], ok=False, error={
                    "code": "invalid_request", "message": "Read-only synthetic panel fixture.",
                })


os.umask(0o077)
with socketserver.ThreadingUnixStreamServer(str(endpoint), Protocol) as server:
    server.daemon_threads = True
    server.serve_forever()
