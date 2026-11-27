//! Single-track analysis of 16 kHz mono audio: VAD → ASR per speech segment
//! → words → diarization → word-to-speaker assignment → speakers with talk
//! time → clean turns → embeddings → centroids (design §6.3, plan §13.11).

use std::collections::BTreeMap;

use broscribe_core::identity::mean_normalized;
use broscribe_core::schema::{Centroid, LanguageSpan, Meeting, Segment, Speaker, Word};
use broscribe_core::{Error, Result, SpeakerId};
use broscribe_models::{AsrToken, DiarSegment, ModelSet, SpeakerEmbedder};

use crate::audio::{TARGET_RATE, slice_s};
use crate::ranges::{self, Range};

/// The single track of an imported file.
pub const TRACK_ID: &str = "t0";
/// Shortest clean turn embedded for a centroid or voiceprint.
pub const MIN_CLEAN_S: f64 = 1.5;
/// Most clean source turns selected per speaker; model limits can subdivide them.
pub const MAX_CLEAN_TURNS: usize = 20;
/// A word with no overlapping turn goes to the nearest turn within this gap.
pub const NEAREST_TURN_S: f64 = 2.0;
/// Length given to the last word when its token has no duration.
pub const DEFAULT_WORD_S: f64 = 0.08;
/// ASR input limit; longer VAD regions are cut into equal pieces.
const MAX_ASR_S: f64 = 30.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalyzeOptions {
    /// `false` skips VAD and ASR: speakers come from diarization only and
    /// `segments` stays empty (calibration).
    pub run_asr: bool,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        Self { run_asr: true }
    }
}

/// A diarized turn with its meeting speaker ID.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Turn {
    pub start: f64,
    pub end: f64,
    pub speaker: SpeakerId,
}

/// Result of [`analyze`]. Speakers are unassigned (`assignment = none`).
#[derive(Clone, Debug, PartialEq)]
pub struct Analysis {
    pub segments: Vec<Segment>,
    pub speakers: Vec<Speaker>,
    /// Diarized turns, sorted by start.
    pub diar_segments: Vec<Turn>,
    pub duration_s: f64,
}

/// One ASR region: its range, its words and the recognised text.
pub type AsrRegion = (Range, Vec<Word>, String);

/// Analyzes one track. Inference only: call it outside the library lock.
pub fn analyze(samples: &[f32], models: &mut ModelSet, opts: AnalyzeOptions) -> Result<Analysis> {
    analyze_with_languages(samples, models, opts, &[])
}

/// [`analyze`] with the meeting's language spans: speech is split at span
/// boundaries and each piece is decoded in its span's language
/// ([`transcribe_speech`]).
pub fn analyze_with_languages(
    samples: &[f32],
    models: &mut ModelSet,
    opts: AnalyzeOptions,
    languages: &[LanguageSpan],
) -> Result<Analysis> {
    let duration_s = samples.len() as f64 / f64::from(TARGET_RATE);
    let asr_regions = if opts.run_asr {
        transcribe_speech(samples, models, languages)?
    } else {
        Vec::new()
    };
    let diar_segments = label_turns(&models.diarizer.diarize(samples)?);
    let segments = segments_from(asr_regions, &diar_segments);
    let speakers = build_speakers(samples, &diar_segments, models.embedder.as_ref())?;
    Ok(Analysis {
        segments,
        speakers,
        diar_segments,
        duration_s,
    })
}

/// VAD, then ASR per speech region (cut at [`MAX_ASR_S`] and at language-span
/// boundaries; each piece decoded with its span's language, none outside
/// the spans).
pub fn transcribe_speech(
    samples: &[f32],
    models: &mut ModelSet,
    languages: &[LanguageSpan],
) -> Result<Vec<AsrRegion>> {
    let mut regions = Vec::new();
    for seg in models.vad.segments(samples)? {
        for (range, lang) in split_at_languages((seg.start_s, seg.end_s), languages) {
            for (start, end) in split_even(range, MAX_ASR_S) {
                let audio = slice_s(samples, start, end);
                let out = match lang {
                    Some(l) => models.asr.transcribe_with(audio, Some(l))?,
                    None => models.asr.transcribe(audio)?,
                };
                let words = tokens_to_words(&out.tokens, start);
                regions.push(((start, end), words, out.text));
            }
        }
    }
    Ok(regions)
}

/// Cuts `range` at language-span boundaries; pieces outside every span
/// have no language. Spans are taken in start order; overlaps go to the
/// earlier span.
fn split_at_languages(range: Range, languages: &[LanguageSpan]) -> Vec<(Range, Option<&str>)> {
    let mut spans: Vec<&LanguageSpan> = languages
        .iter()
        .filter(|s| s.to_s > s.from_s && !s.lang.trim().is_empty())
        .collect();
    spans.sort_by(|a, b| a.from_s.total_cmp(&b.from_s));
    let (mut at, end) = range;
    let mut out = Vec::new();
    for s in spans {
        if at >= end {
            break;
        }
        if s.to_s <= at || s.from_s >= end {
            continue;
        }
        if s.from_s > at {
            out.push(((at, s.from_s), None));
            at = s.from_s;
        }
        let stop = s.to_s.min(end);
        out.push(((at, stop), Some(s.lang.trim())));
        at = stop;
    }
    if at < end {
        out.push(((at, end), None));
    }
    out
}

/// Transcript segments from ASR regions and diarization on one track.
pub fn segments_from(asr_regions: Vec<AsrRegion>, turns: &[Turn]) -> Vec<Segment> {
    segments_with_overlap(asr_regions, turns, &[])
}

/// Reuse saved overlap evidence when re-running ASR without the diarizer.
pub(crate) fn segments_with_overlap(
    asr_regions: Vec<AsrRegion>,
    turns: &[Turn],
    saved_overlap: &[Range],
) -> Vec<Segment> {
    let overlap = ranges::union(
        overlapping_turns(turns)
            .into_iter()
            .chain(saved_overlap.iter().copied()),
    );
    let turns = TurnIndex::new(turns);
    let attribute = |start, end| {
        if overlap_in(&overlap, start, end).next().is_some() {
            Attribution::Overlap
        } else {
            Attribution::Speaker(turns.speaker(start, end))
        }
    };
    let mut segments = Vec::new();
    for ((start, end), words, text) in asr_regions {
        if words.is_empty() {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            // Text without token times: one segment over the region.
            push_segment(
                &mut segments,
                attribute(start, end),
                start,
                end,
                text.to_owned(),
                Vec::new(),
                &overlap,
            );
            continue;
        }
        for (attribution, run) in split_by_attribution(words, attribute) {
            let start = run.first().map_or(start, |w| w.start);
            let end = run.iter().map(|w| w.end).fold(start, f64::max);
            let text = run
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            push_segment(&mut segments, attribution, start, end, text, run, &overlap);
        }
    }
    segments
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Attribution {
    Speaker(Option<SpeakerId>),
    Overlap,
}

/// Intervals with at least two distinct speakers. Union each speaker's turns
/// before counting them so repeated turns of one speaker only count once.
fn overlapping_turns(turns: &[Turn]) -> Vec<Range> {
    let mut by_speaker = BTreeMap::<SpeakerId, Vec<Range>>::new();
    for t in turns {
        by_speaker
            .entry(t.speaker)
            .or_default()
            .push((t.start, t.end));
    }
    let mut events: Vec<_> = by_speaker
        .into_values()
        .flat_map(|spans| {
            ranges::union(spans)
                .into_iter()
                .flat_map(|(a, b)| [(a, 1), (b, -1)])
        })
        .collect();
    events.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let mut active = 0;
    let mut previous = 0.0;
    let mut overlap = Vec::new();
    for (time, delta) in events {
        if active >= 2 && time > previous {
            overlap.push((previous, time));
        }
        previous = time;
        active += delta;
    }
    overlap
}

fn overlap_in(overlap: &[Range], start: f64, end: f64) -> impl Iterator<Item = [f64; 2]> + '_ {
    let from = overlap.partition_point(|&(_, b)| b <= start);
    overlap[from..]
        .iter()
        .take_while(move |&&(a, _)| a < end)
        .filter_map(move |&(a, b)| {
            let (a, b) = (a.max(start), b.min(end));
            (b > a && end > start).then_some([a, b])
        })
}

fn push_segment(
    segments: &mut Vec<Segment>,
    attribution: Attribution,
    start: f64,
    end: f64,
    text: String,
    words: Vec<Word>,
    overlap: &[Range],
) {
    let id = u32::try_from(segments.len()).unwrap_or(u32::MAX);
    let (speaker, speech_overlap) = match attribution {
        Attribution::Speaker(speaker) => (speaker, Vec::new()),
        Attribution::Overlap => (None, overlap_in(overlap, start, end).collect()),
    };
    segments.push(Segment {
        speech_overlap,
        id,
        track: TRACK_ID.to_owned(),
        speaker,
        start,
        end,
        text,
        words,
    });
}

/// Cuts a range longer than `max` into equal pieces of at most `max`.
fn split_even((start, end): Range, max: f64) -> Vec<Range> {
    let len = end - start;
    if len <= max || max <= 0.0 {
        return vec![(start, end)];
    }
    let n = (len / max).ceil();
    let step = len / n;
    (0..n as usize)
        .map(|i| {
            let a = start + step * i as f64;
            let b = if i + 1 == n as usize { end } else { a + step };
            (a, b)
        })
        .collect()
}

/// Assembles BPE tokens into words. A token starting with `▁` or a space
/// starts a new word. A word ends where its last token ends: start +
/// duration, else the next token's start, else +[`DEFAULT_WORD_S`]. Times
/// are shifted by `offset_s` (the segment start).
pub fn tokens_to_words(tokens: &[AsrToken], offset_s: f64) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        let start = f64::from(t.start_s);
        let end = match t.duration_s {
            Some(d) if d > 0.0 => start + f64::from(d),
            _ => tokens
                .get(i + 1)
                .map(|n| f64::from(n.start_s))
                .filter(|&e| e > start)
                .unwrap_or(start + DEFAULT_WORD_S),
        };
        let starts_word = t.text.starts_with(['\u{2581}', ' ']) || words.is_empty();
        let piece = t.text.trim_start_matches(['\u{2581}', ' ']);
        match words.last_mut() {
            Some(last) if !starts_word => {
                last.text.push_str(piece);
                last.end = last.end.max(end);
            }
            // A bare `▁` token opens a word that the next pieces fill.
            Some(last) if last.text.is_empty() => {
                last.text.push_str(piece);
                last.end = last.end.max(end);
            }
            _ => words.push(Word {
                text: piece.to_owned(),
                start,
                end,
            }),
        }
    }
    words.retain(|w| !w.text.trim().is_empty());
    for w in &mut words {
        w.start += offset_s;
        w.end += offset_s;
    }
    words
}

/// Relabels diarizer clusters as `S1, S2, …` in order of first appearance,
/// dropping empty turns; sorted by start.
pub fn label_turns(diar: &[DiarSegment]) -> Vec<Turn> {
    let mut sorted: Vec<&DiarSegment> = diar.iter().filter(|d| d.end_s > d.start_s).collect();
    sorted.sort_by(|a, b| {
        a.start_s
            .total_cmp(&b.start_s)
            .then(a.end_s.total_cmp(&b.end_s))
    });
    let mut labels: Vec<u32> = Vec::new();
    sorted
        .into_iter()
        .map(|d| {
            let n = match labels.iter().position(|&l| l == d.speaker) {
                Some(i) => i + 1,
                None => {
                    labels.push(d.speaker);
                    labels.len()
                }
            };
            Turn {
                start: d.start_s,
                end: d.end_s,
                speaker: SpeakerId::new(u32::try_from(n).unwrap_or(u32::MAX))
                    .expect("labels start at 1"),
            }
        })
        .collect()
}

/// Speaker turns indexed by start with a max-tree of their ends, so the
/// speaker of each word is found among the turns near it instead of by a
/// scan of every turn per word. Turns may come in any order; the answer is
/// the one a scan in input order gives.
struct TurnIndex<'t> {
    turns: &'t [Turn],
    /// Positions of the turns with finite times and `start <= end`, by
    /// start (equal starts keep input order).
    sorted: Vec<usize>,
    /// Implicit binary tree over `sorted`: leaf `leaves + k` holds the end
    /// of `sorted[k]`, every other node the latest end below it.
    max_end: Vec<f64>,
    leaves: usize,
    /// The other turns, compared for every word.
    odd: Vec<usize>,
}

/// How far beyond [`NEAREST_TURN_S`] the nearest-turn query reaches, so
/// rounding never leaves out a turn the exact gap test accepts.
const NEAREST_MARGIN_S: f64 = 1.0;

impl<'t> TurnIndex<'t> {
    fn new(turns: &'t [Turn]) -> Self {
        let (mut sorted, odd): (Vec<usize>, Vec<usize>) = (0..turns.len()).partition(|&i| {
            let t = &turns[i];
            t.start.is_finite() && t.end.is_finite() && t.start <= t.end
        });
        sorted.sort_by(|&a, &b| turns[a].start.total_cmp(&turns[b].start));
        let leaves = sorted.len().next_power_of_two();
        let mut max_end = vec![f64::NEG_INFINITY; 2 * leaves];
        for (k, &i) in sorted.iter().enumerate() {
            max_end[leaves + k] = turns[i].end;
        }
        for node in (1..leaves).rev() {
            max_end[node] = max_end[2 * node].max(max_end[2 * node + 1]);
        }
        Self {
            turns,
            sorted,
            max_end,
            leaves,
            odd,
        }
    }

    /// The speaker of a word or region: the turn with the most overlap, else
    /// the nearest turn within [`NEAREST_TURN_S`], else `None` (unknown
    /// speaker). Ties go to the turn given first.
    fn speaker(&self, start: f64, end: f64) -> Option<SpeakerId> {
        // Other ranges are compared with every turn.
        let regular = start.is_finite() && end.is_finite() && start <= end;
        let mut best: Option<(f64, usize)> = None;
        let mut overlap = |i: usize| {
            let t = &self.turns[i];
            let o = ranges::overlap1((start, end), (t.start, t.end));
            if o > 0.0 && best.is_none_or(|(b, j)| o > b || (o == b && i < j)) {
                best = Some((o, i));
            }
        };
        if regular {
            // Overlap needs `turn.start < end` and `turn.end > start`.
            let hi = self.sorted.partition_point(|&i| self.turns[i].start < end);
            self.each(hi, &|e| e > start, &mut overlap);
            self.odd.iter().copied().for_each(&mut overlap);
        } else {
            (0..self.turns.len()).for_each(&mut overlap);
        }
        if let Some((_, i)) = best {
            return Some(self.turns[i].speaker);
        }
        let mut nearest: Option<(f64, usize)> = None;
        let mut near = |i: usize| {
            let t = &self.turns[i];
            let gap = if end <= t.start {
                t.start - end
            } else if start >= t.end {
                start - t.end
            } else {
                0.0
            };
            if gap <= NEAREST_TURN_S && nearest.is_none_or(|(b, j)| gap < b || (gap == b && i < j))
            {
                nearest = Some((gap, i));
            }
        };
        if regular {
            let reach = NEAREST_TURN_S + NEAREST_MARGIN_S;
            let (from, to) = (start - reach, end + reach);
            let hi = self.sorted.partition_point(|&i| self.turns[i].start <= to);
            self.each(hi, &|e| e >= from, &mut near);
            self.odd.iter().copied().for_each(&mut near);
        } else {
            (0..self.turns.len()).for_each(&mut near);
        }
        nearest.map(|(_, i)| self.turns[i].speaker)
    }

    /// Calls `f` with each of the first `hi` sorted turns whose end `keeps`;
    /// `keeps` must hold for every end later than one it holds for.
    fn each(&self, hi: usize, keeps: &impl Fn(f64) -> bool, f: &mut impl FnMut(usize)) {
        self.visit(1, 0, self.leaves, hi, keeps, f);
    }

    fn visit(
        &self,
        node: usize,
        from: usize,
        to: usize,
        hi: usize,
        keeps: &impl Fn(f64) -> bool,
        f: &mut impl FnMut(usize),
    ) {
        if from >= hi || !keeps(self.max_end[node]) {
            return;
        }
        if node >= self.leaves {
            f(self.sorted[from]);
            return;
        }
        let mid = from + (to - from) / 2;
        self.visit(2 * node, from, mid, hi, keeps, f);
        self.visit(2 * node + 1, mid, to, hi, keeps, f);
    }
}

/// Consecutive words with the same attribution, in order.
fn split_by_attribution(
    words: Vec<Word>,
    attribute: impl Fn(f64, f64) -> Attribution,
) -> Vec<(Attribution, Vec<Word>)> {
    let mut out: Vec<(Attribution, Vec<Word>)> = Vec::new();
    for w in words {
        let s = attribute(w.start, w.end);
        match out.last_mut() {
            Some((cur, run)) if *cur == s => run.push(w),
            _ => out.push((s, vec![w])),
        }
    }
    out
}

/// Speaker IDs in `turns`, in order of first appearance.
pub fn speaker_ids(turns: &[Turn]) -> Vec<SpeakerId> {
    let mut ids: Vec<SpeakerId> = Vec::new();
    for t in turns {
        if !ids.contains(&t.speaker) {
            ids.push(t.speaker);
        }
    }
    ids
}

/// The ranges of one speaker's turns.
pub fn speaker_ranges(turns: &[Turn], speaker: SpeakerId) -> Vec<Range> {
    turns
        .iter()
        .filter(|t| t.speaker == speaker)
        .map(|t| (t.start, t.end))
        .collect()
}

/// A speaker's clean turns: its turns minus every other speaker's turns,
/// pieces of at least [`MIN_CLEAN_S`], the [`MAX_CLEAN_TURNS`] longest, in
/// time order.
pub fn clean_turns(turns: &[Turn], speaker: SpeakerId) -> Vec<Range> {
    let own = speaker_ranges(turns, speaker);
    let others: Vec<Range> = turns
        .iter()
        .filter(|t| t.speaker != speaker)
        .map(|t| (t.start, t.end))
        .collect();
    ranges::longest(
        ranges::subtract(&own, &others),
        MIN_CLEAN_S,
        MAX_CLEAN_TURNS,
    )
}

/// Embeds actual decoded ranges, subdividing only when a model declares an input limit.
pub fn embed_ranges(
    samples: &[f32],
    ranges: &[Range],
    embedder: &dyn SpeakerEmbedder,
) -> Result<Vec<(Range, Vec<f32>)>> {
    let limit = embedder.max_input_samples().unwrap_or(usize::MAX);
    if limit == 0 {
        return Err(Error::Inference(
            "speaker embedder declares zero input capacity".into(),
        ));
    }
    let rate = f64::from(crate::audio::TARGET_RATE);
    let duration = samples.len() as f64 / rate;
    let mut out = Vec::with_capacity(ranges.len());
    for &(a, b) in ranges {
        let a = a.max(0.0).min(duration);
        let b = b.max(a).min(duration);
        let first = ((a * rate) as usize).min(samples.len());
        let last = ((b * rate) as usize).min(samples.len());
        let clip = &samples[first..last];
        if clip.is_empty() {
            continue;
        }
        let count = clip.len().div_ceil(limit);
        let base = clip.len() / count;
        let extra = clip.len() % count;
        out.reserve(count);
        let mut offset = 0;
        for part in 0..count {
            let end = offset + base + usize::from(part < extra);
            if let Some(vector) = embedder.embed(&clip[offset..end])? {
                if vector.len() != embedder.dim() {
                    return Err(Error::Inference(format!(
                        "{} returned {} values, expected {}",
                        embedder.model_id(),
                        vector.len(),
                        embedder.dim()
                    )));
                }
                let start_s = if part == 0 {
                    a
                } else {
                    (first + offset) as f64 / rate
                };
                let end_s = if part + 1 == count {
                    b
                } else {
                    (first + end) as f64 / rate
                };
                out.push(((start_s, end_s), vector));
            }
            offset = end;
        }
    }
    Ok(out)
}

/// Centroid of the embeddings of `ranges`; `None` when nothing could be embedded.
pub fn centroid_of(
    samples: &[f32],
    ranges: &[Range],
    embedder: &dyn SpeakerEmbedder,
) -> Result<Option<Centroid>> {
    let embedded = embed_ranges(samples, ranges, embedder)?;
    let (ranges, vectors): (Vec<_>, Vec<_>) = embedded.into_iter().unzip();
    Ok(mean_normalized(&vectors).map(|c| {
        let mut centroid = Centroid::new(
            embedder.model_id(),
            &c,
            u32::try_from(vectors.len()).unwrap_or(u32::MAX),
        );
        centroid.ranges = ranges
            .into_iter()
            .map(|(a, b)| [a as f32, b as f32])
            .collect();
        centroid
    }))
}

/// One unassigned speaker per diarized cluster, with talk time and the
/// centroid of its clean turns.
pub(crate) fn build_speakers(
    samples: &[f32],
    turns: &[Turn],
    embedder: &dyn SpeakerEmbedder,
) -> Result<Vec<Speaker>> {
    let mut out = Vec::new();
    for id in speaker_ids(turns) {
        let mut s = Speaker::new(id, TRACK_ID);
        s.talk_s = ranges::total(&speaker_ranges(turns, id));
        s.centroid = centroid_of(samples, &clean_turns(turns, id), embedder)?;
        out.push(s);
    }
    Ok(out)
}

/// A stored meeting's turns, one per run of consecutive segments of the same
/// speaker on each track (time-ordered; unassigned segments break that track's run).
/// Used where diarization must not run again (re-embedding, confirmation
/// samples, the embedding-only pass).
pub fn segment_runs(meeting: &Meeting) -> Vec<Turn> {
    let mut segs: Vec<&Segment> = meeting.segments.iter().collect();
    segs.sort_by(|a, b| {
        a.track
            .cmp(&b.track)
            .then(a.start.total_cmp(&b.start))
            .then(a.id.cmp(&b.id))
    });
    let mut out: Vec<Turn> = Vec::new();
    let mut prev: Option<SpeakerId> = None;
    let mut prev_track: Option<&str> = None;
    for s in segs {
        match (s.speaker, out.last_mut()) {
            (Some(sp), Some(last)) if prev == Some(sp) && prev_track == Some(s.track.as_str()) => {
                last.end = last.end.max(s.end)
            }
            (Some(sp), _) => out.push(Turn {
                start: s.start,
                end: s.end,
                speaker: sp,
            }),
            (None, _) => {}
        }
        prev = s.speaker;
        prev_track = Some(s.track.as_str());
    }
    out.retain(|t| t.end > t.start);
    out.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.speaker.cmp(&b.speaker)));
    out
}

/// A stored speaker's clean turns from its segment runs: runs of at least
/// [`MIN_CLEAN_S`], the [`MAX_CLEAN_TURNS`] longest.
pub fn stored_clean_turns(meeting: &Meeting, speaker: SpeakerId) -> Vec<Range> {
    ranges::longest(
        speaker_ranges(&segment_runs(meeting), speaker),
        MIN_CLEAN_S,
        MAX_CLEAN_TURNS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centroid_provenance_keeps_only_successfully_embedded_clean_ranges() {
        let set = crate::testutil::models();
        let samples = crate::testutil::conversation(&[(Some(0), 2.0), (None, 2.0), (Some(1), 2.0)]);
        let centroid = centroid_of(
            &samples,
            &[(0.0, 2.0), (2.0, 4.0), (4.0, 6.0)],
            set.embedder.as_ref(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(centroid.ranges, [[0.0, 2.0], [4.0, 6.0]]);
        assert_eq!(centroid.n_turns, 2);
    }
    use broscribe_core::config::Profile;
    use broscribe_models::fakes::{FakeModelProvider, sine};
    use broscribe_models::{ModelProvider, SpeechSegment, Vad};

    fn tok(text: &str, start: f32, dur: Option<f32>) -> AsrToken {
        AsrToken {
            text: text.into(),
            start_s: start,
            duration_s: dur,
        }
    }

    fn turn(start: f64, end: f64, n: u32) -> Turn {
        Turn {
            start,
            end,
            speaker: SpeakerId::new(n).unwrap(),
        }
    }

    #[test]
    fn words_from_tokens() {
        let tokens = [
            tok("\u{2581}Hel", 0.0, Some(0.2)),
            tok("lo", 0.2, Some(0.1)),
            tok(" there", 0.5, None),
            tok(",", 0.9, None),
            tok("\u{2581}", 1.2, None),
            tok("ok", 1.3, None),
        ];
        let w = tokens_to_words(&tokens, 10.0);
        let texts: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["Hello", "there,", "ok"]);
        assert!((w[0].start - 10.0).abs() < 1e-6 && (w[0].end - 10.3).abs() < 1e-5);
        // "there" ends at the next token's start; "," at the next one's.
        assert!((w[1].end - 11.2).abs() < 1e-5);
        // The last token has no duration and no successor: +0.08 s.
        assert!((w[2].end - (11.3 + DEFAULT_WORD_S)).abs() < 1e-5);
        assert!((w[2].start - 11.2).abs() < 1e-5);
    }

    #[test]
    fn word_to_speaker() {
        let turns = vec![turn(0.0, 2.0, 1), turn(1.5, 4.0, 2), turn(10.0, 11.0, 1)];
        let index = TurnIndex::new(&turns);
        let s = |n| Some(SpeakerId::new(n).unwrap());
        assert_eq!(index.speaker(0.2, 0.5), s(1));
        // More overlap with S2.
        assert_eq!(index.speaker(1.6, 2.5), s(2));
        // No overlap: nearest within 2 s.
        assert_eq!(index.speaker(5.0, 5.2), s(2));
        assert_eq!(index.speaker(8.5, 8.9), s(1));
        // Too far from everyone.
        assert_eq!(index.speaker(6.5, 7.0), None);
        assert_eq!(TurnIndex::new(&[]).speaker(0.0, 1.0), None);
    }

    /// The fallback attribution rule, by a scan of every turn in input
    /// order: the most overlap, else the nearest within `NEAREST_TURN_S`;
    /// the turn given first wins ties.
    fn scanned(start: f64, end: f64, turns: &[Turn]) -> Option<SpeakerId> {
        let mut best: Option<(f64, SpeakerId)> = None;
        for t in turns {
            let o = ranges::overlap1((start, end), (t.start, t.end));
            if o > 0.0 && best.is_none_or(|(b, _)| o > b) {
                best = Some((o, t.speaker));
            }
        }
        if let Some((_, s)) = best {
            return Some(s);
        }
        let mut nearest: Option<(f64, SpeakerId)> = None;
        for t in turns {
            let gap = if end <= t.start {
                t.start - end
            } else if start >= t.end {
                start - t.end
            } else {
                0.0
            };
            if gap <= NEAREST_TURN_S && nearest.is_none_or(|(b, _)| gap < b) {
                nearest = Some((gap, t.speaker));
            }
        }
        nearest.map(|(_, s)| s)
    }

    /// A fixed xorshift sequence: the same synthetic cases on every run.
    struct Cases(u64);

    impl Cases {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }

        /// A time on a quarter-second grid, so overlaps and gaps often tie.
        fn time(&mut self) -> f64 {
            self.below(400) as f64 * 0.25
        }

        /// A length: mostly short, sometimes empty, inverted, long or
        /// not finite.
        fn len(&mut self) -> f64 {
            match self.below(16) {
                0 => 0.0,
                1 => -0.5,
                2 => 60.0,
                3 => f64::INFINITY,
                4 => f64::NAN,
                _ => (1 + self.below(16)) as f64 * 0.25,
            }
        }
    }

    #[test]
    fn turn_lookup_gives_every_word_the_speaker_a_full_scan_gives() {
        let mut cases = Cases(0x9E37_79B9_7F4A_7C15);
        for case in 0..300 {
            // Turns out of order, overlapping, touching and repeated with
            // another speaker.
            let mut turns: Vec<Turn> = (0..cases.below(40))
                .map(|_| {
                    let start = cases.time();
                    let end = start + cases.len();
                    turn(start, end, 1 + cases.below(4) as u32)
                })
                .collect();
            if let Some(&t) = turns.get(cases.below(8) as usize) {
                turns.push(turn(t.start, t.end, 5));
            }
            let index = TurnIndex::new(&turns);
            let mut expected = Vec::new();
            let mut got = Vec::new();
            for _ in 0..1 + cases.below(4) {
                // Words in no particular order.
                let words: Vec<Word> = (0..cases.below(30))
                    .map(|k| {
                        let start = cases.time();
                        Word {
                            text: format!("w{k}"),
                            start,
                            end: start + cases.len(),
                        }
                    })
                    .collect();
                let start = cases.time();
                let range = (start, start + cases.len());
                if words.is_empty() {
                    expected.push(scanned(range.0, range.1, &turns));
                    got.push(index.speaker(range.0, range.1));
                } else {
                    expected.extend(words.iter().map(|w| scanned(w.start, w.end, &turns)));
                    got.extend(words.iter().map(|w| index.speaker(w.start, w.end)));
                }
            }
            assert_eq!(got, expected, "case {case}: {turns:?}");
        }
    }

    #[test]
    fn overlapping_voices_keep_text_without_assigning_a_speaker() {
        let turns = [
            turn(3.0, 8.0, 2),
            turn(0.0, 5.0, 1),
            turn(0.5, 1.5, 1),
            turn(8.0, 9.0, 3),
        ];
        let words = [
            ("before", 1.0, 2.0),
            ("both", 2.5, 4.0),
            ("voices", 4.0, 5.0),
            ("after", 5.0, 6.0),
            ("next", 7.5, 8.5),
        ]
        .map(|(text, start, end)| Word {
            text: text.into(),
            start,
            end,
        });
        let regions = vec![
            (
                (0.0, 9.0),
                words.to_vec(),
                "before both voices after next".into(),
            ),
            ((2.0, 6.0), Vec::new(), "Text without word times.".into()),
        ];
        let segments = segments_from(regions.clone(), &turns);
        let ranges = vec![[3.0, 5.0]];
        assert_eq!(
            segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            [
                "before",
                "both voices",
                "after next",
                "Text without word times."
            ]
        );
        assert_eq!(segments[0].speaker, Some(turns[1].speaker));
        assert_eq!(segments[1].speaker, None);
        assert_eq!(segments[1].words, words[1..3]);
        assert_eq!(segments[1].speech_overlap, ranges);
        assert_eq!(segments[2].speaker, Some(turns[0].speaker));
        assert_eq!(segments[3].speaker, None);
        assert_eq!(segments[3].speech_overlap, ranges);

        let stored_turns = [turn(1.0, 2.0, 1), turn(5.0, 8.5, 2)];
        assert_eq!(
            segments_with_overlap(regions, &stored_turns, &[(3.0, 5.0)]),
            segments
        );
    }

    #[test]
    fn clean_turns_drop_overlap_and_short_pieces() {
        let turns = vec![
            turn(0.0, 3.0, 1),
            turn(2.0, 4.0, 2),
            turn(5.0, 6.0, 1),
            turn(7.0, 12.0, 1),
        ];
        let s1 = SpeakerId::new(1).unwrap();
        assert_eq!(clean_turns(&turns, s1), vec![(0.0, 2.0), (7.0, 12.0)]);
        let many: Vec<Turn> = (0..30)
            .map(|i| turn(i as f64 * 3.0, i as f64 * 3.0 + 2.0, 1))
            .collect();
        assert_eq!(clean_turns(&many, s1).len(), MAX_CLEAN_TURNS);
    }

    #[test]
    fn labels_follow_first_appearance() {
        let d = |a, b, s| DiarSegment {
            start_s: a,
            end_s: b,
            speaker: s,
        };
        let t = label_turns(&[d(5.0, 6.0, 0), d(0.0, 1.0, 3), d(2.0, 2.0, 1)]);
        assert_eq!(t, vec![turn(0.0, 1.0, 1), turn(5.0, 6.0, 2)]);
    }

    #[test]
    fn analyze_two_tones() {
        let mut audio = sine(200.0, 4.0, 0.5);
        audio.extend(sine(440.0, 4.0, 0.5));
        let provider = FakeModelProvider {
            schedule: vec![
                DiarSegment {
                    start_s: 0.0,
                    end_s: 4.0,
                    speaker: 7,
                },
                DiarSegment {
                    start_s: 4.0,
                    end_s: 8.0,
                    speaker: 2,
                },
            ],
            asr_words: vec![
                "alpha".into(),
                "beta".into(),
                "gamma".into(),
                "delta".into(),
            ],
            ..FakeModelProvider::default()
        };
        let mut models = provider.load(&Profile::default()).unwrap();
        let a = analyze(&audio, &mut models, AnalyzeOptions::default()).unwrap();
        assert_eq!(a.speakers.len(), 2);
        assert_eq!(a.speakers[0].id.to_string(), "S1");
        assert!((a.speakers[0].talk_s - 4.0).abs() < 1e-9);
        let c1 = a.speakers[0].centroid.as_ref().unwrap();
        let c2 = a.speakers[1].centroid.as_ref().unwrap();
        assert_eq!(c1.model, "nemo-titanet-large");
        assert_ne!(c1.vector().unwrap(), c2.vector().unwrap());
        // The VAD finds one 8 s region; its words split at the speaker change.
        assert_eq!(a.segments.len(), 2);
        assert_eq!(a.segments[0].text, "alpha beta");
        assert_eq!(a.segments[1].speaker, SpeakerId::new(2).ok());

        let b = analyze(&audio, &mut models, AnalyzeOptions { run_asr: false }).unwrap();
        assert!(b.segments.is_empty());
        assert_eq!(b.speakers, a.speakers);
    }

    #[test]
    fn long_regions_are_cut_for_asr() {
        struct OneRegion;
        impl Vad for OneRegion {
            fn segments(&mut self, _: &[f32]) -> Result<Vec<SpeechSegment>> {
                Ok(vec![SpeechSegment {
                    start_s: 0.0,
                    end_s: 70.0,
                }])
            }
        }
        let mut models = FakeModelProvider::default()
            .load(&Profile::default())
            .unwrap();
        models.vad = Box::new(OneRegion);
        let audio = sine(200.0, 70.0, 0.5);
        let a = analyze(&audio, &mut models, AnalyzeOptions::default()).unwrap();
        // 3 pieces × 2 words, all one speaker → one segment per piece.
        assert_eq!(a.segments.len(), 3);
        assert!(a.segments.iter().all(|s| s.end - s.start <= MAX_ASR_S));
    }
}

#[cfg(test)]
mod multitrack_turn_tests {
    use super::*;
    #[test]
    fn multitrack_interleaved_other_track_does_not_break_clean_turns() {
        let mut meeting = Meeting::new(
            broscribe_core::MeetingId::parse("m-000001").unwrap(),
            broscribe_core::schema::Source::Recording,
            broscribe_core::schema::now(),
        );
        for (id, track, speaker, start, end) in [
            (0, "mic", 1, 0.0, 0.8),
            (1, "system", 2, 0.4, 0.7),
            (2, "mic", 1, 0.8, 1.6),
            (3, "system", 2, 1.2, 1.5),
        ] {
            meeting.segments.push(Segment {
                speech_overlap: Vec::new(),
                id,
                track: track.into(),
                speaker: Some(SpeakerId::new(speaker).unwrap()),
                start,
                end,
                text: "Synthetic".into(),
                words: Vec::new(),
            });
        }
        assert_eq!(
            stored_clean_turns(&meeting, SpeakerId::new(1).unwrap()),
            vec![(0.0, 1.6)]
        );
    }
}

#[cfg(test)]
mod bounded_embedding_tests {
    use super::*;
    use broscribe_models::fakes::{ToneEmbedder, sine};

    #[test]
    fn bounded_centroids_cover_just_over_capacity_and_many_parts() {
        let mut model = ToneEmbedder::new("bounded", 16);
        model.max_samples = Some(2 * 16_000);
        for seconds in [2.25, 7.25] {
            let audio = sine(200.0, seconds, 0.5);
            let parts = embed_ranges(&audio, &[(0.0, seconds)], &model).unwrap();
            assert_eq!(parts.len(), if seconds < 3.0 { 2 } else { 4 });
            assert_eq!(parts.first().unwrap().0.0, 0.0);
            assert_eq!(parts.last().unwrap().0.1, seconds);
            assert!(parts.windows(2).all(|p| p[0].0.1 == p[1].0.0));
            assert!(parts.iter().all(|((a, b), _)| b - a <= 2.0));
            let centroid = centroid_of(&audio, &[(0.0, seconds)], &model)
                .unwrap()
                .unwrap();
            let expected = ToneEmbedder::new("bounded", 16)
                .embed(&sine(200.0, 1.0, 0.5))
                .unwrap()
                .unwrap();
            assert!(
                broscribe_core::identity::cosine(&centroid.vector().unwrap(), &expected) > 0.99
            );
        }
    }
}
