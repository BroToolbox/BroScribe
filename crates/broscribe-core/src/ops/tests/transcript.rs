//! The one transcript projection (CL06): order, labels, provenance, merged
//! lines and filters exactly as the CLI, MCP and the app read them, and the
//! prompt projection that cannot carry a name.

use super::*;
use crate::render::{NameStyle, PeopleLookup, format_hms};
use crate::schema::{GroupOrigin, SpeakerGroup, UtterancePerson};
use crate::transcript::{
    AttributionSource, TranscriptFilter, Who, format_line, line_fingerprints, merge_lines,
    prompt_transcript, resolve_who, transcript,
};

fn at(id: u32, track: &str, speaker: Option<u32>, start: f64, text: &str) -> Segment {
    Segment {
        speech_overlap: Vec::new(),
        id,
        track: track.into(),
        speaker: speaker.map(sid),
        start,
        end: start + 2.0,
        text: text.into(),
        words: Vec::new(),
    }
}

const BIA_NAME: &str = "Bia <Admin>*";

fn people() -> Vec<Person> {
    vec![
        Person::new(pid(ANA), "Ana Example"),
        Person::new(pid(BIA), BIA_NAME),
    ]
}

/// S1 is Ana (confirmed), S2 unnamed with member S4 merged into it, S3 the
/// unnamed microphone; one S2 line was corrected to Bia by live decision 7;
/// one segment is blank and one has no speaker. Segments are stored out of
/// order.
fn sample() -> Meeting {
    let mut ana = Speaker::new(sid(1), "t0");
    ana.person_id = Some(pid(ANA));
    ana.assignment = Assignment::Confirmed;
    let mut m = meeting(MA, 1, Vec::new());
    m.speakers = vec![
        ana,
        Speaker::new(sid(2), "t0"),
        Speaker::new(sid(3), "mic"),
        Speaker::new(sid(4), "t0"),
    ];
    m.speaker_groups = vec![SpeakerGroup::new(sid(2), [sid(4)], GroupOrigin::User)];
    m.segments = vec![
        at(6, "t0", Some(2), 10.0, "corrected line"),
        at(0, "t0", Some(1), 0.0, "Hello there"),
        at(2, "t0", Some(2), 4.0, "pricing"),
        at(1, "t0", Some(1), 2.0, "  second  "),
        at(5, "t0", Some(1), 1.0, "   "),
        at(3, "t0", Some(4), 6.0, "member says"),
        at(4, "mic", Some(3), 8.0, "from the mic"),
        at(7, "t0", None, 12.0, "nobody"),
    ];
    let corrected = segment_fingerprint(&m.segments[0]);
    m.utterance_people.push(UtterancePerson {
        track: corrected.track,
        start: corrected.start,
        end: corrected.end,
        text_sha256: corrected.text_sha256,
        person_id: pid(BIA),
        at: schema::now(),
        live_decision: Some(7),
    });
    m
}

fn ids(segments: &[crate::transcript::TranscriptSegment<'_>]) -> Vec<u32> {
    segments.iter().map(|s| s.segment_id).collect()
}

#[test]
fn every_front_end_reads_one_order_label_and_provenance() {
    let m = sample();
    let people = PeopleLookup::new(people());
    let rows = transcript(&m, &people, &TranscriptFilter::default());

    assert_eq!(
        ids(&rows),
        vec![0, 1, 2, 3, 4, 6, 7],
        "blank dropped, by start"
    );
    let labels: Vec<&str> = rows.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "Ana Example",
            "Ana Example",
            "Speaker 2",
            "Speaker 2",
            "Unknown (mic)",
            BIA_NAME,
            "Unknown"
        ]
    );
    assert_eq!(rows[1].text, "second");

    let member = &rows[3];
    assert_eq!(member.speaker, Some(sid(2)));
    assert_eq!(member.original_speaker, Some(sid(4)));
    assert_eq!(member.diarized_speaker(), Some(sid(4)));
    assert_eq!(rows[2].original_speaker, None);

    let ana = &rows[0];
    assert_eq!(ana.attribution, AttributionSource::Speaker);
    assert_eq!(ana.assignment, Some(Assignment::Confirmed));
    assert_eq!(ana.person_id, Some(pid(ANA)));
    assert_eq!(ana.person_name.as_deref(), Some("Ana Example"));

    let corrected = &rows[5];
    assert_eq!(corrected.attribution, AttributionSource::Line);
    assert_eq!(corrected.attribution.as_str(), "line");
    assert_eq!(corrected.line_person(), Some(&pid(BIA)));
    assert_eq!(corrected.live_decision, Some(7));
    assert_eq!(
        corrected.speaker_label, "Speaker 2",
        "the speaker is untouched"
    );

    let nobody = &rows[6];
    assert_eq!((nobody.speaker, nobody.assignment), (None, None));
}

#[test]
fn lines_merge_one_speaker_and_split_at_a_correction() {
    let m = sample();
    let people = PeopleLookup::new(people());
    let lines = merge_lines(&transcript(&m, &people, &TranscriptFilter::default()));
    let shape: Vec<(Vec<u32>, &str)> = lines
        .iter()
        .map(|l| (l.segment_ids.clone(), l.label.as_str()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (vec![0, 1], "Ana Example"),
            (vec![2, 3], "Speaker 2"),
            (vec![4], "Unknown (mic)"),
            (vec![6], BIA_NAME),
            (vec![7], "Unknown"),
        ]
    );
    assert_eq!(lines[0].text, "Hello there second");
    assert_eq!(lines[1].end, 8.0);
    assert_eq!(lines[3].line_person, Some(pid(BIA)));
    assert_eq!(lines[3].person_id, Some(pid(BIA)));

    let fps = line_fingerprints(&m, &lines[1].segment_ids).unwrap();
    assert_eq!(fps.len(), 2);
    assert_eq!(fps[1], segment_fingerprint(&m.segments[5]));
    assert!(line_fingerprints(&m, &[2, 99]).is_none());
}

#[test]
fn overlap_is_visible_without_losing_the_words_or_assigning_a_name() {
    let mut m = sample();
    let mut overlap = at(8, "t0", None, 14.0, "Two voices here.");
    overlap.speech_overlap.push([14.5, 15.0]);
    m.segments.push(overlap);
    let m: Meeting = serde_json::from_value(serde_json::to_value(m).unwrap()).unwrap();
    let people = PeopleLookup::new(people());
    let rows = transcript(&m, &people, &TranscriptFilter::default());
    let row = rows.last().unwrap();
    assert!(row.overlapping_speech);
    assert_eq!((row.speaker, &row.person_id), (None, &None));
    assert_eq!(row.text, "Two voices here.");
    let lines = merge_lines(&rows);
    assert_eq!(lines[lines.len() - 2].label, "Unknown");
    assert_eq!(lines.last().unwrap().label, "Overlapping speech");
    assert!(
        crate::render::render_meeting_md(&m, &people)
            .unwrap()
            .contains("[00:00:14] Overlapping speech: Two voices here.")
    );
}

#[test]
fn names_are_raw_until_written_in_a_style() {
    let m = sample();
    let people = PeopleLookup::new(people());
    let lines = merge_lines(&transcript(&m, &people, &TranscriptFilter::default()));
    let bia = &lines[3];
    assert_eq!(bia.label, BIA_NAME, "the projection keeps the stored name");
    let stamp = format_hms(10.0);
    assert_eq!(
        bia.display(NameStyle::Markdown),
        format!("[{stamp}] Bia \\<Admin\\>\\*: corrected line")
    );
    assert_eq!(
        format_line(bia.start, &bia.label, &bia.text, NameStyle::Raw),
        format!("[{stamp}] {BIA_NAME}: corrected line")
    );
}

#[test]
fn one_filter_rule_for_speakers_people_labels_and_times() {
    let m = sample();
    let (_tmp, lib) = setup(people().into_iter().map(|p| (p, vec![])).collect(), vec![]);
    let people = PeopleLookup::new(lib.list_people().unwrap());
    let pick = |reference: &str| -> Vec<u32> {
        let who = resolve_who(&lib, &m, &people, reference).unwrap();
        let filter = TranscriptFilter {
            who: Some(who),
            ..TranscriptFilter::default()
        };
        ids(&transcript(&m, &people, &filter))
    };

    assert_eq!(pick("S2"), vec![2, 3, 6], "a group's lines, as shown");
    assert_eq!(pick("S4"), vec![3], "a member's own lines");
    assert_eq!(pick("ana example"), vec![0, 1]);
    assert_eq!(pick(BIA_NAME), vec![6], "lines corrected to her");
    assert_eq!(pick(&pid(ANA).to_string()), vec![0, 1]);
    assert_eq!(pick(" speaker 2 "), vec![2, 3], "a shown label");
    assert_eq!(pick("Unknown (mic)"), vec![4]);
    assert!(matches!(
        resolve_who(&lib, &m, &people, "S9"),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        resolve_who(&lib, &m, &people, "Nobody Here"),
        Err(Error::NotFound(_))
    ));
    assert_eq!(
        resolve_who(&lib, &m, &people, "S4").unwrap(),
        Who::Speaker {
            id: sid(4),
            own_lines_only: true
        }
    );

    let window = TranscriptFilter {
        who: None,
        from_s: Some(5.0),
        to_s: Some(9.0),
    };
    assert_eq!(ids(&transcript(&m, &people, &window)), vec![2, 3, 4]);
}

#[test]
fn prompt_transcript_has_no_names_only_opaque_labels() {
    let m = sample();
    let prompt = prompt_transcript(&m);
    let labels: Vec<&str> = prompt.segments.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(labels, vec!["S1", "S1", "S2", "S2", "S3", "P1", "S?"]);
    assert_eq!(prompt.line_people.len(), 1);
    assert_eq!(prompt.line_people["P1"], pid(BIA));
    let everything = format!("{prompt:?}");
    for name in ["Ana Example", "Bia", "Admin"] {
        assert!(!everything.contains(name), "{name} reached the prompt");
    }
}
