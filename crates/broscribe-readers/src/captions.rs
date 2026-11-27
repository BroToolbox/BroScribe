//! Caption revision tracking, independent of the AX transport; name
//! normalisation lives in [`broscribe_core::names`].
use crate::CaptionEvent;
pub use broscribe_core::names::{folded_name, normalize_name};
use broscribe_core::schema::sha256_hex;
use std::collections::HashMap;

fn rolling_revision(old: &str, new: &str) -> bool {
    if new.starts_with(old) || old.starts_with(new) {
        return true;
    }
    let old: Vec<_> = old.split_whitespace().collect();
    let new: Vec<_> = new.split_whitespace().collect();
    (2..=old.len().min(new.len())).any(|n| old[old.len() - n..] == new[..n])
}

#[derive(Clone, Debug)]
pub struct RawCaption {
    pub id: Option<String>,
    pub speaker_name: String,
    pub text: String,
}

/// Caption revision state for one observation epoch (one reader start, until
/// pause, stop or reader restart). Revisions are monotonic per caption for the
/// tracker's whole life: an id that scrolls away and comes back continues its
/// chain. Emitted ids are namespaced by a fresh epoch, so a later tracker in
/// the same recording (resume, accessory restart) never reuses an id whose
/// revision counter it cannot know; alignment then treats both epochs' views
/// of one utterance as votes on the same ASR span, newest observation first.
pub struct CaptionTracker {
    /// Last emitted event per internal key; never pruned during the epoch.
    latest: HashMap<String, CaptionEvent>,
    visible: Vec<String>,
    sequence: u64,
    epoch: String,
}

impl Default for CaptionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptionTracker {
    pub fn new() -> Self {
        Self::with_epoch(new_epoch())
    }

    fn with_epoch(epoch: String) -> Self {
        Self {
            latest: HashMap::new(),
            visible: Vec::new(),
            sequence: 0,
            epoch,
        }
    }

    /// One visible caption snapshot. No app id: reuse a visible utterance only
    /// for an exact or rolling-prefix update; first-seen time and a sequence
    /// disambiguate repeated identical utterances in later snapshots.
    /// `folded_self_labels` are already [`folded_name`]s (the engine folds
    /// them once per alias change, not per line).
    pub fn update(
        &mut self,
        raw: Vec<RawCaption>,
        observed_s: f64,
        folded_self_labels: &[String],
    ) -> Vec<CaptionEvent> {
        let mut visible: Vec<String> = Vec::new();
        let mut changed = Vec::new();
        for line in raw {
            let text = line.text.split_whitespace().collect::<Vec<_>>().join(" ");
            let name = normalize_name(&line.speaker_name);
            if text.is_empty() || name.is_empty() {
                continue;
            }
            let folded = folded_name(&name);
            // A duplicated app id within one snapshot cannot name two lines;
            // the repeat is tracked by content like an id-less line.
            let app_key = line
                .id
                .filter(|id| !id.is_empty())
                .map(|id| format!("app:{id}"))
                .filter(|key| !visible.contains(key));
            let key = app_key.unwrap_or_else(|| {
                let existing = self.visible.iter().find(|key| {
                    if visible.contains(*key) {
                        return false;
                    }
                    let old = &self.latest[*key];
                    old.text == text
                        || (folded_name(&old.speaker_name) == folded
                            && rolling_revision(&old.text, &text))
                });
                existing.cloned().unwrap_or_else(|| {
                    self.sequence += 1;
                    format!("seen:{folded}:{}:{}", observed_s.floor(), self.sequence)
                })
            });
            visible.push(key.clone());
            let is_self = folded_self_labels.contains(&folded);
            let old = self.latest.get(&key);
            if old.is_some_and(|old| {
                old.text == text && old.speaker_name == name && old.is_self == is_self
            }) {
                continue;
            }
            let event = CaptionEvent {
                caption_id: old.map_or_else(
                    || sha256_hex(format!("{}:{key}", self.epoch).as_bytes()),
                    |o| o.caption_id.clone(),
                ),
                revision: old.map_or(1, |o| o.revision + 1),
                speaker_name: name,
                is_self,
                text,
                observed_s,
                start_s: old.map_or((observed_s - 2.0).max(0.0), |o| o.start_s),
                end_s: observed_s,
            };
            self.latest.insert(key, event.clone());
            changed.push(event);
        }
        self.visible = visible;
        changed
    }
}

/// Unique per tracker within this process and, through the process id and
/// clock, across app or accessory restarts within one recording.
fn new_epoch() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{}:{nanos}:{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn raw(id: Option<&str>, name: &str, text: &str) -> RawCaption {
        RawCaption {
            id: id.map(str::to_owned),
            speaker_name: name.into(),
            text: text.into(),
        }
    }
    #[test]
    fn repeated_and_rolling_snapshots_are_one_revision_chain() {
        let mut t = CaptionTracker::default();
        let first = t.update(
            vec![raw(None, "Você", "hello world")],
            3.0,
            &[folded_name("Você")],
        );
        assert!(first[0].is_self);
        assert!(
            t.update(vec![raw(None, "Você", "hello world")], 4.0, &[])
                .len()
                == 1
        );
        let next = t.update(vec![raw(None, "Você", "hello world again")], 5.0, &[]);
        assert_eq!(first[0].caption_id, next[0].caption_id);
        assert_eq!(next[0].start_s, first[0].start_s);
        assert!(
            t.update(vec![raw(None, "Você", "hello world again")], 6.0, &[])
                .is_empty()
        );
        let rolled = t.update(vec![raw(None, "Você", "world again tomorrow")], 7.0, &[]);
        assert_eq!(rolled[0].caption_id, first[0].caption_id);
    }
    #[test]
    fn speaker_revision_replaces_same_app_caption() {
        let mut t = CaptionTracker::default();
        t.update(vec![raw(Some("line"), "Daria", "hello world")], 3.0, &[]);
        let revised = t.update(vec![raw(Some("line"), "Niko", "hello world")], 4.0, &[]);
        assert_eq!(revised[0].revision, 2);
        assert_eq!(revised[0].speaker_name, "Niko");
    }

    fn named(events: &[CaptionEvent]) -> Vec<String> {
        use crate::alignment::{AsrSpan, CaptionAligner, Setup, Track};
        let mut aligner = CaptionAligner::default();
        for event in events {
            aligner.ingest(event.clone());
        }
        let span = AsrSpan {
            id: "system:1".into(),
            speaker_id: broscribe_core::SpeakerId::new(1).unwrap(),
            track: Track::System,
            start_s: 0.0,
            end_s: 5.0,
            text: "alpha beta gamma delta".into(),
        };
        aligner
            .names(&[span], Setup::Headphones)
            .into_iter()
            .map(|n| n.name)
            .collect()
    }

    #[test]
    fn caption_that_disappears_and_returns_continues_its_revision_chain() {
        let mut t = CaptionTracker::default();
        let mut events = t.update(
            vec![raw(Some("line"), "Daria", "alpha beta gamma delta")],
            3.0,
            &[],
        );
        assert!(t.update(Vec::new(), 4.0, &[]).is_empty());
        // Scrolled back into view unchanged: nothing new to report.
        assert!(
            t.update(
                vec![raw(Some("line"), "Daria", "alpha beta gamma delta")],
                5.0,
                &[]
            )
            .is_empty()
        );
        let returned = t.update(
            vec![raw(Some("line"), "Niko", "alpha beta gamma delta")],
            6.0,
            &[],
        );
        assert_eq!(returned[0].caption_id, events[0].caption_id);
        assert_eq!(returned[0].revision, 2);
        assert_eq!(returned[0].start_s, events[0].start_s);
        events.extend(returned);
        assert_eq!(named(&events), ["Niko"]);
    }

    #[test]
    fn resumed_reader_never_reuses_an_earlier_epochs_caption_ids() {
        // Before pause.
        let mut before = CaptionTracker::default();
        let mut events = before.update(
            vec![
                raw(Some("line"), "Daria", "alpha beta gamma delta"),
                raw(None, "Daria", "zeta eta theta iota"),
            ],
            3.2,
            &[],
        );
        drop(before);
        // After resume the timeline continues from the pause point and the
        // app reuses its id; a fresh tracker starts every chain at 1.
        let mut after = CaptionTracker::default();
        let resumed = after.update(
            vec![
                raw(Some("line"), "Niko", "alpha beta gamma delta"),
                raw(None, "Daria", "kappa lambda mu nu"),
            ],
            3.7,
            &[],
        );
        for (old, new) in events.iter().zip(&resumed) {
            assert_ne!(old.caption_id, new.caption_id);
            assert_eq!(new.revision, 1);
        }
        events.extend(resumed);
        // The newer observation of the same utterance wins its ASR span.
        assert_eq!(named(&events), ["Niko"]);
    }
}
