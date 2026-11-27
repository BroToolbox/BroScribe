"""Create synthetic meetings and people from scratch, without models."""
from datetime import datetime, timedelta, timezone
from hashlib import sha256
import json
import os
from pathlib import Path
import subprocess

data = Path(os.environ["BROSCRIBE_DATA_DIR"])
library = Path(os.environ["BROSCRIBE_LIBRARY"])
seed = datetime(2026, 11, 24, 9, tzinfo=timezone.utc)

data.mkdir(parents=True)
library.mkdir(parents=True)
(data / "config.toml").write_text(
    f'library = "{library}"\ndefault_language = "en"\n'
    '[app]\nopen_at_login = false\n'
    '[setup]\ncompleted_steps = ["assistant"]\n'
    '[appearance]\nreading_size = "largest"\n'
)
staging = library.parent / "incoming"
staging.mkdir()
audio = staging / "invented-tone.m4a"
subprocess.run(
    ["ffmpeg", "-v", "error", "-f", "lavfi", "-i",
     "sine=frequency=440:sample_rate=48000:duration=40", str(audio)],
    check=True,
)
audio_hash = sha256(audio.read_bytes()).hexdigest()

for number in range(120):
    at = seed - timedelta(hours=number * 4)
    stamp = at.isoformat()
    handle = f"{number:06x}"
    title = "Moonberry planning rehearsal" if number == 0 else f"Moonberry rehearsal {number:03}"
    folder = library / at.strftime("%Y/%m") / f"{at:%Y-%m-%d-%H%M}-moonberry-{handle}"
    folder.mkdir(parents=True)
    speakers = [
        {"id": f"S{i + 1}", "track": "mixed", "talk_s": 10, "assignment": "none"}
        for i in range(6)
    ]
    turns = [
        {"id": i, "track": "mixed", "speaker": f"S{i % 6 + 1}",
         "start": i * 2.0, "end": i * 2.0 + 1.5,
         "text": f"Invented line {i + 1}. The Moonberry crew will review "
                 "the cardboard observatory plans and compare the paper telescope designs."}
        for i in range(18 if number == 0 else 2)
    ]
    meeting = {
        "schema_version": 2, "id": f"m-{handle}", "source": "import", "title": title,
        "started_at": stamp, "created_at": stamp, "updated_at": stamp,
        "duration_s": 40, "tags": ["synthetic", "moonberry"],
        "speakers": speakers if number < 2 else [],
        "tracks": [{"id": "mixed", "role": "mixed"}],
        "segments": turns if number < 2 else [dict(turn, speaker=None) for turn in turns],
        "processing": {"status": "done"},
    }
    if number == 0:
        meeting["review_notes"] = [{"source": "reprocess", "text": "Check the invented telescope handoff."}]
        meeting["recording"] = {
            "state": "available", "handling": "copy", "path": "audio.m4a",
            "sha256": audio_hash, "bytes": audio.stat().st_size,
        }
        (folder / "audio.m4a").write_bytes(audio.read_bytes())
    (folder / "meeting.json").write_text(json.dumps(meeting) + "\n")
    (folder / "meeting.md").write_text(f"# {title}\n\nInvented QA meeting.\n")

for number in range(40):
    person = library / "people" / f"p-{number:06x}"
    person.mkdir(parents=True)
    (person / "person.md").write_text(
        f'---\nid: {person.name}\nname: Moonberry Person {number + 1:02}\n'
        f'created_at: "{seed.isoformat()}"\n---\n\nInvented person for QA.\n'
    )

subprocess.run(["broscribe", "index", "rebuild", "--json"], check=True)
