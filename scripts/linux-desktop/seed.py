"""Create synthetic meetings and people from scratch, without models.

The default mode is the QA matrix's stress library: 120 numbered meetings,
40 people and the Largest text size. `--showcase` builds the README
screenshot library from showcase.py instead: invented meetings with named
speakers and generated notes, at the default text size.
"""
import argparse
from datetime import datetime, timedelta, timezone
from hashlib import sha256
import json
import os
from pathlib import Path
import stat
import subprocess
import sys

data = Path(os.environ["BROSCRIBE_DATA_DIR"])
library = Path(os.environ["BROSCRIBE_LIBRARY"])


def matrix():
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


# The synthetic agent CLI the showcase enables as its only provider. It runs
# locally, reads the prompt on stdin and answers with the invented notes of
# the meeting whose first line the prompt contains; it never uses a network.
NOTES_CLI = """#!/usr/bin/env python3
import json, sys
args = sys.argv[1:]
if args[:1] == ["--version"]:
    print("showcase-notes 1.0.0"); sys.exit(0)
if "--help" in args:
    print("  -p, --print\\n  --json-schema <schema>\\n  --tools <tools>"); sys.exit(0)
prompt = sys.stdin.read()
for entry in json.load(open(REPLIES)):
    if entry["match"] in prompt:
        print(json.dumps({"structured_output": entry["reply"]})); sys.exit(0)
sys.exit("showcase-notes: no invented notes for this transcript")
"""

NOTES_PROVIDER = """[[provider]]
id = "showcase-notes"
name = "Local notes model"
vendor = "Nobody (synthetic, offline)"
runtime = "agent-cli"
binary = "showcase-notes"
prompt_transport = "stdin"
output_keys = ["structured_output"]
args = [
  { flag = "-p", need = "required" },
  { flag = "--json-schema", values = ["{schema}"], need = "required" },
  { flag = "--tools", values = [""], need = "safety" },
]
"""


def clock_env(start):
    """The CLI's environment on the pinned clock, when libfaketime is given."""
    env = dict(os.environ)
    lib = os.environ.get("SHOWCASE_FAKETIME_LIB")
    if lib:
        env.update(LD_PRELOAD=lib, FAKETIME=f"@{start}", FAKETIME_DONT_FAKE_MONOTONIC="1",
                   FAKETIME_NO_CACHE="1", TZ="UTC")
    return env


def cli(env, *args, stdin=None):
    result = subprocess.run(["broscribe", "--json", *args], env=env, input=stdin,
                            capture_output=True, text=True)
    if result.returncode != 0:
        sys.exit(f"broscribe {' '.join(args)} failed ({result.returncode}):\n{result.stdout}{result.stderr}")
    return json.loads(result.stdout) if result.stdout.strip() else None


def showcase():
    import showcase as show

    env = clock_env(show.SEED_CLOCK)
    data.mkdir(parents=True)
    library.mkdir(parents=True)
    (data / "config.toml").write_text(
        f'library = "{library}"\ndefault_language = "en"\nauto_enrich = "off"\n'
        '[app]\nopen_at_login = false\n'
        '[setup]\ncompleted_steps = ["assistant"]\n'
        '[updates]\nenabled = false\n'
    )
    for person_id, name in show.PEOPLE.values():
        person = library / "people" / person_id
        person.mkdir(parents=True)
        (person / "person.md").write_text(
            f'---\nid: {person_id}\nname: {name}\n'
            f'created_at: "2026-10-01T09:00:00+00:00"\n---\n\nInvented volunteer.\n'
        )

    for spec in show.MEETINGS:
        at = datetime.fromisoformat(spec["start"])
        duration = spec["minutes"] * 60.0
        speakers = [f"S{index + 1}" for index in range(len(spec["people"]))]
        lines = spec["lines"]
        segments = []
        for index, (start, speaker, text) in enumerate(lines):
            end = min(start + max(2.5, len(text.split()) * 0.38), duration)
            segments.append({"id": index, "track": "mixed", "speaker": speaker,
                             "start": float(start), "end": round(end, 2), "text": text})
        # Talk time over the whole meeting, shared like the excerpt's words.
        words = {speaker: 1 for speaker in speakers}
        for _, speaker, text in lines:
            words[speaker] += len(text.split())
        total = sum(words.values())
        stamp = at.isoformat()
        meeting = {
            "schema_version": 2, "id": spec["id"], "source": "recording",
            "title": spec["title"], "title_source": "user",
            "started_at": stamp, "created_at": stamp, "updated_at": stamp,
            "duration_s": duration, "app": spec["app"], "tags": spec["tags"],
            "languages": [{"lang": "en", "from_s": 0.0, "to_s": duration}],
            "speakers": [{"id": speaker, "track": "mixed", "assignment": "none",
                          "talk_s": round(duration * 0.9 * words[speaker] / total, 1)}
                         for speaker in speakers],
            "tracks": [{"id": "mixed", "role": "mixed"}],
            "segments": segments,
            "processing": {"status": "done"},
        }
        folder = library / at.strftime("%Y/%m") / f"{at:%Y-%m-%d-%H%M}-{spec['id']}"
        folder.mkdir(parents=True)
        if spec["id"] == show.FEATURED:
            # Generated silence of the meeting's length, so the player shows.
            audio = folder / "audio.m4a"
            subprocess.run(
                ["ffmpeg", "-v", "error", "-f", "lavfi", "-i", "anullsrc=r=16000:cl=mono",
                 "-t", str(duration), "-c:a", "aac", "-b:a", "12k", "-map_metadata", "-1",
                 "-fflags", "+bitexact", "-flags:a", "+bitexact", str(audio)],
                check=True,
            )
            meeting["recording"] = {
                "state": "available", "handling": "copy", "path": audio.name,
                "sha256": sha256(audio.read_bytes()).hexdigest(), "bytes": audio.stat().st_size,
            }
        (folder / "meeting.json").write_text(json.dumps(meeting, indent=1) + "\n")
    cli(env, "index", "rebuild")

    # Speaker names through the identity code path: confirmed people, so the
    # transcript carries names and People counts meetings and talk time.
    for spec in show.MEETINGS:
        for index, key in enumerate(spec["people"]):
            cli(env, "speakers", "name", spec["id"], f"S{index + 1}", show.PEOPLE[key][0])

    # Notes through the enrichment code path, from the synthetic provider.
    bin_dir = Path.home() / ".local/bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    replies = data / "showcase-notes.json"
    replies.write_text(json.dumps([{"match": spec["lines"][0][2], "reply": spec["notes"]}
                                   for spec in show.MEETINGS]))
    notes_cli = bin_dir / "showcase-notes"
    notes_cli.write_text(NOTES_CLI.replace("REPLIES", json.dumps(str(replies)), 1))
    notes_cli.chmod(stat.S_IRWXU)
    (data / "models.toml").write_text(NOTES_PROVIDER)
    cli(env, "providers", "enable", "showcase-notes", "--accept")
    for spec in show.MEETINGS:
        cli(env, "enrich", spec["id"], "--provider", "showcase-notes")
    cli(env, "index", "rebuild")


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--showcase", action="store_true", help="README screenshot library")
if parser.parse_args().showcase:
    showcase()
else:
    matrix()
