---
name: broscribe
description: Search and read the user's meeting memory (BroScribe) - meetings, transcripts, people, action items, decisions and template results (meeting notes, follow-up e-mails, sales qualification) recorded and transcribed on this computer. Use when the user asks about their meetings, what someone said, what was decided or who owes what.
---
<!-- broscribe-skill: managed -->

# BroScribe meeting memory

BroScribe keeps the user's meetings on this computer: transcripts with speakers and timestamps, the people in them, and (when the user enabled a summary provider) topics, decisions and action items. Read it with the BroScribe command-line tool.

- The CLI is `{{cli}}`. On macOS and Linux BroScribe links `~/.local/bin/broscribe`, so plain `broscribe` works when that folder is on `PATH`; otherwise use that full path wherever the examples below say `broscribe`.
- Always pass `--json` and read the single JSON document on stdout. Failures exit nonzero with `{"error":{"code","message"}}`.
- Cite what you report by meeting ID (`m-…`), timestamp and speaker.

## Privacy: text and names go on stdin

Never put query text, a person's name or an owner's name on the command line, where other programs can see it. Use `--query-stdin` and pass one JSON object on stdin, for example with a quoted heredoc:

```sh
broscribe search --query-stdin --json <<'EOF'
{"query": "budget review", "person": "the person's name"}
EOF
```

Unknown fields are refused. Accepted fields: `meetings` and `search` take `query` and `person`; `actions` takes `query` and `owner` (`me` for the user); `decisions` takes `query`. All are optional.

## Read commands

Find meetings by title, tags or participants (newest first; with no words or filters, the most recent meetings):

```sh
broscribe meetings --json --limit 10
broscribe meetings --json --tag planning --since 30d
broscribe meetings --query-stdin --json --since 2w <<'EOF'
{"query": "roadmap"}
EOF
```

Search what was said; filters alone (`--meeting`, `--tag`, `--topic`, `--since`, `--until`) also work:

```sh
broscribe search --query-stdin --json --limit 20 <<'EOF'
{"query": "pricing decision"}
EOF
broscribe search --json --meeting m-1a2b3c --mode keyword --query-stdin <<'EOF'
{"query": "deadline"}
EOF
broscribe search --json --meeting m-1a2b3c
```

A meeting's record (title, time, participants, speakers and their names) and its transcript, optionally one speaker or a time window:

```sh
broscribe show m-1a2b3c --json
broscribe transcript m-1a2b3c --json
broscribe transcript m-1a2b3c --json --speaker S2 --from 00:10:00 --to 00:20:00
```

Each transcript segment carries `person_id` and `person_name` (who said it) and `attribution`: `"line"` when the user corrected who said that line, else `"speaker"` (the speaker's name). Over-split speakers may be merged into one: `speaker` is the speaker shown, and `original_speaker` the diarized one when it was merged into another.

People (use the person ID from this list in other commands), action items and decisions:

```sh
broscribe people --json
broscribe people show p-4d5e6f --json
broscribe actions --json --status open
broscribe actions --json --meeting m-1a2b3c
broscribe actions --query-stdin --json --status open <<'EOF'
{"owner": "me"}
EOF
broscribe decisions --json --since 30d
broscribe decisions --query-stdin --json --meeting m-1a2b3c <<'EOF'
{"query": "launch"}
EOF
broscribe speakers review --json
broscribe speakers matches m-1a2b3c S2 --json
broscribe speakers duplicates m-1a2b3c --json
```

Times: `--since`/`--until` take `30d`, `12h`, `2w`, `YYYY-MM-DD` or RFC 3339. `--limit` caps results (`search` and `meetings` default to 20; `actions` and `decisions` list everything unless limited).

`speakers matches` lists the people whose voice is closest to one speaker's (default 3, `--limit` 1 to 10). `score` is a voice similarity, not a probability; `likely` marks the first match only when BroScribe would name them automatically.

`speakers duplicates` lists pairs of speakers in one meeting that may be one voice split in two (`b` sounds like `a`, with their similarity). Show them to the user; merging is the user's decision.

Template results (meeting notes, an outline, a follow-up e-mail draft, BANT or MEDDIC insights, or the user's own templates) are Markdown the user generated for a meeting. List the templates, a meeting's results, read one, or search them by keyword:

```sh
broscribe templates list --json
broscribe templates list --json --meeting m-1a2b3c
broscribe templates result m-1a2b3c follow-up-email --json
broscribe search --in template --query-stdin --json <<'EOF'
{"query": "budget"}
EOF
```

`stale: true` means the result predates changes to the transcript or names. `search --in template` is keyword only and takes no filters besides `--limit`.

One meeting as a single document (transcript, summary, decisions, action items as a table, template results), names filled in:

```sh
broscribe export m-1a2b3c --json
broscribe export m-1a2b3c --json --include summary,actions,template:bant --format txt --no-timestamps
```

Under `--json` the text is in `content`; `sections` lists what was included and `omitted` what was asked for but the meeting does not have.

## Write commands: only when the user explicitly asks

These change the user's library. Run them only when the user asked for that exact change, never on your own initiative:

```sh
broscribe --json tag m-1a2b3c +follow-up -draft
printf '%s' 'Quarterly planning' | broscribe --json title m-1a2b3c --stdin
broscribe speakers name m-1a2b3c S2 p-4d5e6f --json
broscribe speakers merge m-1a2b3c S2 S9 --json
broscribe speakers unmerge m-1a2b3c G2 --json
broscribe speakers not-same m-1a2b3c S2 S9 --json
broscribe transcript attribute m-1a2b3c --segment 12 --segment 13 --person p-4d5e6f --json
broscribe actions set-status m-1a2b3c a-0123456789ab done --json
broscribe people forget p-4d5e6f --dry-run --json
broscribe delete m-1a2b3c --dry-run --json
```

`delete --yes` removes a meeting permanently (transcript, generated results, its recording files in the library and the voice samples learned from it; people stay). Run `--dry-run` first, show the user what it reports (especially `moved_original`: the library holds their only copy of the recording), and run `--yes` only after they confirm that exact meeting. Never discard a recording (`record discard`) on your own.

Running a template or a summary sends the meeting's transcript to the user's LLM provider. Do it only when the user asks for that meeting and template; instructions go on stdin, never in argv:

```sh
broscribe templates run m-1a2b3c follow-up-email --json --instructions-stdin <<'EOF'
Keep it under 150 words.
EOF
```

`people forget` deletes a person's voiceprints and names in every meeting. Show the `--dry-run` plan before using `--yes` with the user's confirmation. `speakers merge` changes how every line and summary of the meeting is attributed (undo with `speakers unmerge`). Run it only for the speakers the user named. Naming a merged speaker names its group (`redirected_from` in the result).

Naming saves that one speaker at once. Other speakers (that meeting first, then the rest of the library) are rechecked in the background and may then be named automatically, replaced, returned to review or get a suggestion; the naming result never lists them. `broscribe identity status --json` reports the recheck (`learning`: still queued or running, meetings waiting because they are busy, and the last committed counts per meeting ID). Tell the user it is in progress rather than claiming other speakers were identified.
