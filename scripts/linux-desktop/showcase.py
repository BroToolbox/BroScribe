"""Invented meetings for the README screenshots.

Everything here is fiction: the Moonberry volunteer astronomy club, its
people, sponsor and school, and every line of dialogue. Speaker S1, S2…
of a meeting is the n-th person in its `people` list. Times are UTC and
lie before NOW, the pinned clock the screenshots run at.
"""

NOW = "2026-11-24 10:30:00"
# Seeding and enrichment run a little earlier on the same pinned clock.
SEED_CLOCK = "2026-11-24 10:00:00"
FEATURED = "m-5b0001"

PEOPLE = {
    "ana": ("p-5a0001", "Ana Lindqvist"),
    "bea": ("p-5a0002", "Bea Marlowe"),
    "cleo": ("p-5a0003", "Cleo Ferreira"),
    "dev": ("p-5a0004", "Dev Okonkwo"),
    "elif": ("p-5a0005", "Elif Sandberg"),
    "felix": ("p-5a0006", "Felix Arango"),
    "mina": ("p-5a0007", "Mina Calloway"),
    "theo": ("p-5a0008", "Theo Brandt"),
    "priya": ("p-5a0009", "Priya Halvorsen"),
    "rosa": ("p-5a000a", "Rosa Achterberg"),
}


def action(text, at_s, owner, due=None):
    return {"text": text, "at_s": at_s, "owner_speaker": owner, "owner_name": None,
            "due": due, "status": "open"}


def decision(text, at_s, by):
    return {"text": text, "at_s": at_s, "by_speaker": by, "by_name": None}


MEETINGS = [
    {
        "id": FEATURED,
        "start": "2026-11-24T09:00:00+00:00",
        "minutes": 38,
        "title": "Observatory Night run of show",
        "app": "meet",
        "tags": ["events"],
        "people": ["ana", "bea", "cleo", "dev"],
        "lines": [
            (4, "S1", "Morning, everyone. Let's lock the run of show for Observatory Night so the volunteers get it by Friday."),
            (12, "S2", "Sounds good. The community hall confirmed Saturday the fifth, doors at six, and we have the field until ten."),
            (24, "S1", "Great. Cleo, where are we with the kids' telescope workshop?"),
            (29, "S3", "I have forty cardboard telescope kits cut and bagged. The workshop starts at half past six, before it's dark enough to observe."),
            (41, "S4", "The sign-up form had sixty-two families as of last night, so forty kits might be tight."),
            (49, "S3", "I can run two sessions of twenty instead, at half past six and quarter past seven."),
            (57, "S1", "Let's do that. Dev, can you split the form into those two time slots?"),
            (63, "S4", "Yes, I'll have it live today and send a note to everyone who already signed up."),
            (250, "S2", "On telescopes: we have four trained volunteers, but one of them is away that weekend."),
            (259, "S1", "Then let's run three stations, Saturn, the Moon and the Pleiades, and keep the fourth scope as a spare."),
            (268, "S3", "Saturn sets early that night, so it should be the first station people see."),
            (276, "S1", "Good point. Saturn by the entrance, the Moon in the middle, the Pleiades at the far end of the field."),
            (545, "S4", "What's the plan if it's cloudy? Last year we only found out at five o'clock."),
            (553, "S2", "The hall is free on the twelfth as well. I can hold it as our cloud date."),
            (560, "S1", "Let's make the twelfth the backup and decide by noon on the day, based on the forecast."),
            (568, "S4", "I'll put the backup date on the sign-up page so nobody is surprised."),
            (880, "S3", "For safety we need red light only on the field. I'd like to hand out red wristband lights at the gate."),
            (890, "S2", "The sponsor money covers that, and Lumen & Lathe said we can put their banner by the telescopes."),
            (898, "S1", "Cleo, can you order the wristbands and print the field signs?"),
            (903, "S3", "Sure, I'll have both done by Wednesday."),
            (1290, "S1", "Last thing: parking. The school lot opens at half past five, and we need two people directing cars."),
            (1299, "S2", "I'll ask the Saturday stargazing group. A few of them offered to help."),
            (1305, "S1", "Perfect. I'll write up the run of show tonight and send it to all volunteers by Friday."),
            (1312, "S4", "Can you include the map with the station layout?"),
            (1316, "S1", "Will do. Thanks, everyone, this is going to be a great night."),
        ],
        "notes": {
            "summary": "{{S1}} and the team locked the run of show for Observatory Night on Saturday 5 December at the community hall, with 12 December held as the cloud date. The kids' telescope workshop will run as two sessions of twenty to match demand, and three telescope stations will show Saturn, the Moon and the Pleiades. {{S3}} is handling red wristband lights and field signs, and {{S2}} is finding volunteers to direct parking.",
            "topics": ["Run of show", "Kids' telescope workshop", "Telescope stations", "Weather backup", "Safety and parking"],
            "keywords": ["Observatory Night", "cloud date", "red light", "sign-up form"],
            "tags": ["volunteers"],
            "decisions": [
                decision("Run the kids' workshop as two sessions of twenty, at 18:30 and 19:15", 49, "S1"),
                decision("Run three telescope stations: Saturn, the Moon and the Pleiades", 259, "S1"),
                decision("Hold 12 December as the cloud date and decide by noon on the day", 560, "S1"),
            ],
            "actions": [
                action("Split the sign-up form into two workshop time slots and tell registered families", 63, "S4", "2026-11-24"),
                action("Order red wristband lights and print the field signs", 898, "S3", "2026-11-25"),
                action("Ask the Saturday stargazing group for two parking volunteers", 1299, "S2"),
                action("Send the run of show with the station map to all volunteers", 1305, "S1", "2026-11-27"),
                action("Add the cloud date to the sign-up page", 568, "S4"),
            ],
        },
    },
    {
        "id": "m-5b0002", "start": "2026-11-24T08:30:00+00:00", "minutes": 14,
        "title": "Volunteer stand-up", "app": "zoom", "tags": [],
        "people": ["ana", "felix", "mina"],
        "lines": [
            (3, "S1", "Quick round today. Felix, how did the mirror recoating go?"),
            (9, "S2", "Both mirrors are back and look clean. I'll collimate the big Dobsonian on Thursday."),
            (18, "S3", "The December newsletter draft is ready for review, and the Observatory Night poster is at the printer."),
            (27, "S1", "Nice. I'm meeting the events group at nine to finalise the run of show."),
            (35, "S2", "Can someone help me carry the scopes to the hall next week?"),
            (41, "S3", "I can bring the van on Wednesday evening."),
        ],
        "notes": {
            "summary": "Quick status round: the recoated mirrors are back, the December newsletter draft and the Observatory Night poster are ready, and {{S3}} will drive the telescopes to the hall.",
            "topics": ["Status updates"], "keywords": ["mirrors", "newsletter"], "tags": [],
            "decisions": [],
            "actions": [action("Collimate the large Dobsonian telescope", 9, "S2", "2026-11-26"),
                        action("Bring the van to move the telescopes to the hall", 41, "S3", "2026-11-25")],
        },
    },
    {
        "id": "m-5b0003", "start": "2026-11-23T16:00:00+00:00", "minutes": 27,
        "title": "Telescope workshop debrief", "app": "teams", "tags": ["workshops"],
        "people": ["felix", "cleo", "theo"],
        "lines": [
            (5, "S1", "Thanks for staying late. What worked in Saturday's telescope workshop?"),
            (12, "S2", "The cardboard tubes were great, but the lens glue took far too long to dry."),
            (21, "S3", "Pre-gluing the eyepieces the night before would save us twenty minutes."),
            (30, "S1", "Agreed. Let's also print a one-page guide with photos for each step."),
            (39, "S2", "I'll draft it and send it round for comments by Thursday."),
        ],
        "notes": {
            "summary": "The workshop went well, but drying glue slowed it down. Eyepieces will be pre-glued and {{S2}} is writing a one-page photo guide.",
            "topics": ["Workshop feedback"], "keywords": ["eyepieces", "guide"], "tags": [],
            "decisions": [decision("Pre-glue the eyepieces before each workshop", 21, "S1")],
            "actions": [action("Draft a one-page photo guide for the telescope build", 39, "S2", "2026-11-26")],
        },
    },
    {
        "id": "m-5b0004", "start": "2026-11-23T13:30:00+00:00", "minutes": 44,
        "title": "Sponsor call: Lumen & Lathe Optics", "app": "zoom", "tags": ["sponsors"],
        "people": ["ana", "bea", "priya"],
        "lines": [
            (6, "S3", "We loved last year's event and we'd like to sponsor Observatory Night again."),
            (15, "S1", "That's wonderful news. This year we're adding a second kids' workshop."),
            (24, "S3", "We can lend two solar telescopes for the afternoon, with a staff member to run them."),
            (34, "S2", "That would let us open the field at four for daytime observing."),
            (44, "S1", "I'll send the updated programme and banner sizes this week."),
        ],
        "notes": {
            "summary": "Lumen & Lathe Optics will sponsor Observatory Night again and lend two solar telescopes with a staff member, so the field can open at four for daytime observing.",
            "topics": ["Sponsorship", "Solar observing"], "keywords": ["solar telescopes", "banner"], "tags": [],
            "decisions": [decision("Open the field at 16:00 for solar observing", 34, "S2")],
            "actions": [action("Send the updated programme and banner sizes to the sponsor", 44, "S1", "2026-11-27")],
        },
    },
    {
        "id": "m-5b0005", "start": "2026-11-23T10:00:00+00:00", "minutes": 21,
        "title": "Website copy review", "app": "slack", "tags": [],
        "people": ["dev", "mina"],
        "lines": [
            (4, "S2", "I rewrote the home page intro so it leads with the events, not the history of the club."),
            (13, "S1", "Much better. The join button should go above the fold as well."),
            (22, "S2", "And the events page needs the accessibility notes for the field."),
        ],
        "notes": {
            "summary": "The new home page copy leads with events. The join button moves up and the events page gets accessibility notes.",
            "topics": ["Website"], "keywords": ["home page"], "tags": [],
            "decisions": [],
            "actions": [action("Add accessibility notes to the events page", 22, "S1")],
        },
    },
    {
        "id": "m-5b0006", "start": "2026-11-21T11:00:00+00:00", "minutes": 18,
        "title": "Saturday stargazing logistics", "app": "meet", "tags": ["events"],
        "people": ["felix", "theo"],
        "lines": [
            (3, "S1", "Clear skies are forecast after eight, so tonight is on."),
            (10, "S2", "I'll open the gate at half past seven and set out the red lamps."),
            (18, "S1", "Bring the star charts too, we had more beginners than usual last time."),
        ],
        "notes": {
            "summary": "Tonight's stargazing goes ahead. {{S2}} opens the gate at 19:30 with red lamps and star charts for beginners.",
            "topics": ["Stargazing evening"], "keywords": ["forecast"], "tags": [],
            "decisions": [], "actions": [],
        },
    },
    {
        "id": "m-5b0007", "start": "2026-11-20T15:00:00+00:00", "minutes": 52,
        "title": "Grant application working session", "app": "zoom", "tags": ["funding"],
        "people": ["ana", "elif", "bea"],
        "lines": [
            (8, "S1", "The community fund deadline is the first of December, so let's finish the budget section today."),
            (17, "S2", "The equipment line is four thousand two hundred, mostly the new mount and two eyepiece sets."),
            (28, "S3", "We should add the outreach costs, buses for two school visits."),
            (37, "S2", "Good catch. That brings us to just under five thousand."),
        ],
        "notes": {
            "summary": "The team drafted the community fund budget: equipment, a new mount and eyepieces, plus buses for two school visits, just under five thousand in total.",
            "topics": ["Grant budget"], "keywords": ["community fund", "equipment"], "tags": [],
            "decisions": [decision("Include outreach transport in the grant budget", 37, "S2")],
            "actions": [action("Finish the grant narrative and circulate it", 8, "S1", "2026-11-28")],
        },
    },
    {
        "id": "m-5b0008", "start": "2026-11-20T10:30:00+00:00", "minutes": 25,
        "title": "1:1 Ana / Theo", "app": "meet", "tags": [],
        "people": ["ana", "theo"],
        "lines": [
            (4, "S1", "How are your first weeks with the club going?"),
            (10, "S2", "Really well. I'd like to learn to run a telescope station on my own."),
            (19, "S1", "Let's pair you with Felix at the next two stargazing evenings."),
        ],
        "notes": {
            "summary": "{{S2}} is settling in and wants to run a telescope station. {{S1}} will pair them with an experienced volunteer for two evenings.",
            "topics": ["Mentoring"], "keywords": [], "tags": [],
            "decisions": [], "actions": [],
        },
    },
    {
        "id": "m-5b0009", "start": "2026-11-19T14:00:00+00:00", "minutes": 16,
        "title": "December newsletter planning", "app": "teams", "tags": [],
        "people": ["mina", "dev"],
        "lines": [
            (3, "S1", "Lead story is Observatory Night, then the Geminids on the thirteenth."),
            (11, "S2", "I can add a sign-up button that links straight to the form."),
        ],
        "notes": {
            "summary": "The December issue leads with Observatory Night, followed by the Geminid meteor shower.",
            "topics": ["Newsletter"], "keywords": ["Geminids"], "tags": [],
            "decisions": [], "actions": [action("Add a sign-up button to the newsletter", 11, "S2")],
        },
    },
    {
        "id": "m-5b000a", "start": "2026-11-19T09:30:00+00:00", "minutes": 33,
        "title": "Cardboard dome build review", "app": "zoom", "tags": ["workshops"],
        "people": ["felix", "cleo", "ana"],
        "lines": [
            (5, "S1", "The dome panels fit, but the slit is too narrow for the bigger telescope."),
            (14, "S2", "We could widen it to forty centimetres and add a second brace."),
            (24, "S3", "Let's test the wider slit at the next build day before cutting all the panels."),
        ],
        "notes": {
            "summary": "The cardboard dome fits together, but the slit needs widening to forty centimetres. The team will test it before cutting every panel.",
            "topics": ["Dome build"], "keywords": ["slit", "brace"], "tags": [],
            "decisions": [decision("Test a 40 cm slit before cutting all panels", 24, "S3")],
            "actions": [],
        },
    },
    {
        "id": "m-5b000b", "start": "2026-11-18T13:00:00+00:00", "minutes": 29,
        "title": "School outreach: Juniper Lane Primary", "app": "teams", "tags": ["outreach"],
        "people": ["cleo", "rosa"],
        "lines": [
            (6, "S2", "Year five is studying the solar system next term, and the class would love a visit."),
            (15, "S1", "We can bring the planet scale model and run the cardboard telescope workshop."),
            (24, "S2", "The second week of January works best for us."),
        ],
        "notes": {
            "summary": "Juniper Lane Primary invited the club for a solar system visit in the second week of January, with the scale model and the telescope workshop.",
            "topics": ["School visit"], "keywords": ["solar system"], "tags": [],
            "decisions": [], "actions": [action("Propose two January dates to the school", 24, "S1")],
        },
    },
    {
        "id": "m-5b000c", "start": "2026-11-17T18:00:00+00:00", "minutes": 41,
        "title": "New volunteer onboarding", "app": "zoom", "tags": ["volunteers"],
        "people": ["ana", "theo", "mina", "felix"],
        "lines": [
            (5, "S1", "Welcome! Tonight covers safety on the field, the telescopes and how we run events."),
            (16, "S4", "Rule one: no white light after dark, and never point anything at the Sun without a filter."),
            (27, "S2", "Is there a rota for the stargazing evenings?"),
            (33, "S3", "Yes, I'll add you to the shared calendar tonight."),
        ],
        "notes": {
            "summary": "New volunteers learned the field safety rules and how events run, and were added to the stargazing rota.",
            "topics": ["Onboarding", "Safety"], "keywords": ["rota"], "tags": [],
            "decisions": [], "actions": [action("Add the new volunteers to the shared calendar", 33, "S3")],
        },
    },
    {
        "id": "m-5b000d", "start": "2026-11-17T11:00:00+00:00", "minutes": 19,
        "title": "Budget check-in", "app": "meet", "tags": ["funding"],
        "people": ["elif", "ana"],
        "lines": [
            (4, "S1", "We're on track for the year, with about six hundred left for events."),
            (12, "S2", "Good, that covers the wristband lights and the printing for Observatory Night."),
        ],
        "notes": {
            "summary": "The budget is on track, and the remaining events money covers the Observatory Night supplies.",
            "topics": ["Budget"], "keywords": [], "tags": [],
            "decisions": [], "actions": [],
        },
    },
    {
        "id": "m-5b000e", "start": "2026-11-12T17:00:00+00:00", "minutes": 58,
        "title": "Monthly all-hands", "app": "zoom", "tags": [],
        "people": ["ana", "bea", "cleo", "dev", "elif", "felix", "mina"],
        "lines": [
            (6, "S1", "Welcome to the November all-hands. Big month ahead with Observatory Night."),
            (15, "S5", "Membership is up to eighty-four, and the finances are healthy."),
            (24, "S6", "Both large telescopes are going in for mirror recoating this week."),
            (33, "S7", "The newsletter now reaches over six hundred people."),
            (42, "S2", "The hall is booked for the fifth of December."),
        ],
        "notes": {
            "summary": "Membership reached eighty-four, the mirrors are being recoated, the newsletter reaches over six hundred people and the hall is booked for Observatory Night.",
            "topics": ["Club updates"], "keywords": ["membership"], "tags": [],
            "decisions": [], "actions": [],
        },
    },
]

# The recording panel's synthetic state: a paused meeting, so the
# elapsed time on screen does not tick.
RECORDING = {
    "title": "Telescope loan agreement",
    "started_at": "2026-11-24T10:05:00+00:00",
    "elapsed_s": 1094.0,
}
# Live preview lines of that recording: (start_s, track, person or None for
# the user on the microphone, text).
# Four lines fit the controls' transcript without cutting one at the top.
LIVE_LINES = [
    (1033, "system", "priya", "Our technician will bring the filters and stay until sunset."),
    (1045, "mic", None, "Great. I'll add your team to the volunteer briefing and send the site map."),
    (1060, "system", "priya", "Could you also share the insurance details for the loan?"),
    (1071, "mic", None, "Yes, Elif will email the certificate by Thursday."),
]
