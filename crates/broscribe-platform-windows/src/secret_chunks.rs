//! Secrets larger than one Credential Manager password (2,560 bytes of
//! UTF-16, about 1,280 characters), such as OAuth token bundles.
//!
//! A small secret is stored as is under its key. A larger one (or one that
//! starts like a manifest) is split into chunk entries named after a fresh
//! generation, written first; then the key's own entry is replaced with a
//! manifest naming that generation, which switches readers over in one
//! write. Chunks of the previous generation are deleted afterwards.
//!
//! Writers and deleters of one key are serialized across processes
//! ([`Slots::exclusive`]). Before touching chunks, a writer records every
//! generation it may leave behind in the key's pending entry; the next
//! writer or deleter removes exactly those chunks unless the manifest names
//! them, so a crash at any step leaves neither a torn value nor orphans
//! beyond the next change. Cleanup only deletes chunk names it recorded,
//! never by prefix guessing. A reader that races a replacement re-reads
//! the manifest; length and checksum catch a mixed read.

/// UTF-16 code units per stored value; under the 1,280-unit limit.
pub(crate) const MAX_UNITS: usize = 1024;
const MANIFEST: &str = "broscribe-secret-chunks:v1:";
const PENDING: &str = "broscribe-secret-pending:v1:";
const READ_ATTEMPTS: usize = 3;
/// Most chunks one secret may use (about 2 MB of text).
const MAX_CHUNKS: usize = 2048;

/// The credential slots a store offers (Credential Manager, or a fake).
pub(crate) trait Slots {
    type Error;
    /// Held while one key is changed; released on drop.
    type Guard;
    fn get(&self, key: &str) -> Result<Option<String>, Self::Error>;
    fn set(&self, key: &str, value: &str) -> Result<(), Self::Error>;
    /// Deleting a missing entry succeeds.
    fn delete(&self, key: &str) -> Result<(), Self::Error>;
    /// Exclusive, cross-process ownership of `key`'s entries.
    fn exclusive(&self, key: &str) -> Result<Self::Guard, Self::Error>;
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChunkError<E> {
    Store(E),
    /// A manifest or chunk is malformed or missing.
    Corrupt(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Manifest {
    generation: String,
    count: usize,
    units: usize,
    checksum: u64,
}
impl Manifest {
    fn parse(value: &str) -> Option<Self> {
        let mut parts = value.strip_prefix(MANIFEST)?.split(':');
        let manifest = Self {
            generation: parts.next().filter(|g| valid_generation(g))?.to_owned(),
            count: parts.next()?.parse().ok().filter(|n| *n <= MAX_CHUNKS)?,
            units: parts.next()?.parse().ok()?,
            checksum: u64::from_str_radix(parts.next()?, 16).ok()?,
        };
        parts.next().is_none().then_some(manifest)
    }
    fn render(&self) -> String {
        format!(
            "{MANIFEST}{}:{}:{}:{:016x}",
            self.generation, self.count, self.units, self.checksum
        )
    }
}

/// Chunk generations a change may leave behind: `(generation, count)`.
type Pending = Vec<(String, usize)>;

fn render_pending(pending: &[(String, usize)]) -> String {
    let entries: Vec<String> = pending
        .iter()
        .map(|(generation, count)| format!("{generation}:{count}"))
        .collect();
    format!("{PENDING}{}", entries.join(","))
}

/// `None` for a malformed record: it names nothing that may be deleted.
fn parse_pending(value: &str) -> Option<Pending> {
    value
        .strip_prefix(PENDING)?
        .split(',')
        .map(|entry| {
            let (generation, count) = entry.split_once(':')?;
            let count = count.parse().ok().filter(|n| *n <= MAX_CHUNKS)?;
            valid_generation(generation).then(|| (generation.to_owned(), count))
        })
        .collect()
}

fn valid_generation(generation: &str) -> bool {
    !generation.is_empty() && generation.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn chunk_key(key: &str, generation: &str, index: usize) -> String {
    format!("{key}#chunk-{generation}-{index}")
}

fn pending_key(key: &str) -> String {
    format!("{key}#pending")
}

/// `value` cut on character boundaries into pieces of at most `max_units`
/// UTF-16 code units.
fn split(value: &str, max_units: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let (mut start, mut units) = (0, 0);
    for (index, character) in value.char_indices() {
        if units + character.len_utf16() > max_units {
            pieces.push(&value[start..index]);
            start = index;
            units = 0;
        }
        units += character.len_utf16();
    }
    if start < value.len() {
        pieces.push(&value[start..]);
    }
    pieces
}

/// FNV-1a: integrity of a reassembled value and a stable, non-revealing
/// lock name per key (not a security boundary).
pub(crate) fn checksum(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Reads `key`, reassembling a chunked secret.
pub(crate) fn read<S: Slots>(slots: &S, key: &str) -> Result<Option<String>, ChunkError<S::Error>> {
    let mut last = ChunkError::Corrupt("secret changed while it was read");
    for _ in 0..READ_ATTEMPTS {
        let Some(stored) = slots.get(key).map_err(ChunkError::Store)? else {
            return Ok(None);
        };
        if !stored.starts_with(MANIFEST) {
            return Ok(Some(stored));
        }
        let manifest =
            Manifest::parse(&stored).ok_or(ChunkError::Corrupt("unreadable secret manifest"))?;
        match assemble(slots, key, &manifest)? {
            Some(value) => return Ok(Some(value)),
            // A chunk vanished or did not match: a writer replaced the
            // secret meanwhile; read its new manifest.
            None => last = ChunkError::Corrupt("secret chunks are missing or damaged"),
        }
    }
    Err(last)
}

fn assemble<S: Slots>(
    slots: &S,
    key: &str,
    manifest: &Manifest,
) -> Result<Option<String>, ChunkError<S::Error>> {
    let mut value = String::new();
    for index in 0..manifest.count {
        match slots
            .get(&chunk_key(key, &manifest.generation, index))
            .map_err(ChunkError::Store)?
        {
            Some(chunk) => value.push_str(&chunk),
            None => return Ok(None),
        }
    }
    let intact =
        value.encode_utf16().count() == manifest.units && checksum(&value) == manifest.checksum;
    Ok(intact.then_some(value))
}

fn current_manifest<S: Slots>(
    slots: &S,
    key: &str,
) -> Result<Option<Manifest>, ChunkError<S::Error>> {
    Ok(slots
        .get(key)
        .map_err(ChunkError::Store)?
        .and_then(|stored| Manifest::parse(&stored)))
}

fn delete_chunks<S: Slots>(
    slots: &S,
    key: &str,
    generation: &str,
    count: usize,
) -> Result<(), ChunkError<S::Error>> {
    for index in 0..count {
        slots
            .delete(&chunk_key(key, generation, index))
            .map_err(ChunkError::Store)?;
    }
    Ok(())
}

/// Removes the chunks an interrupted change recorded, except those the
/// committed manifest names. Call with the key's exclusive guard held.
fn recover<S: Slots>(slots: &S, key: &str) -> Result<(), ChunkError<S::Error>> {
    let pending = pending_key(key);
    let Some(record) = slots.get(&pending).map_err(ChunkError::Store)? else {
        return Ok(());
    };
    if let Some(entries) = parse_pending(&record) {
        let live = current_manifest(slots, key)?.map(|m| m.generation);
        for (generation, count) in entries {
            if live.as_deref() != Some(generation.as_str()) {
                delete_chunks(slots, key, &generation, count)?;
            }
        }
    }
    slots.delete(&pending).map_err(ChunkError::Store)
}

/// Stores `value` under `key`; `generation` (ASCII alphanumeric, fresh per
/// write) names its chunks when it needs them.
pub(crate) fn write<S: Slots>(
    slots: &S,
    key: &str,
    value: &str,
    generation: &str,
) -> Result<(), ChunkError<S::Error>> {
    if !valid_generation(generation) {
        return Err(ChunkError::Corrupt("invalid secret generation"));
    }
    let _guard = slots.exclusive(key).map_err(ChunkError::Store)?;
    recover(slots, key)?;
    let previous = current_manifest(slots, key)?.filter(|p| p.generation != generation);
    let units = value.encode_utf16().count();
    let chunked = units > MAX_UNITS || value.starts_with(MANIFEST);
    let pieces = if chunked {
        split(value, MAX_UNITS)
    } else {
        Vec::new()
    };
    if pieces.len() > MAX_CHUNKS {
        return Err(ChunkError::Corrupt("secret is too large"));
    }
    let mut pending: Pending = previous
        .iter()
        .map(|p| (p.generation.clone(), p.count))
        .collect();
    if chunked {
        pending.push((generation.to_owned(), pieces.len()));
    }
    let record = pending_key(key);
    if !pending.is_empty() {
        slots
            .set(&record, &render_pending(&pending))
            .map_err(ChunkError::Store)?;
    }
    if chunked {
        let committed = pieces
            .iter()
            .enumerate()
            .try_for_each(|(index, piece)| slots.set(&chunk_key(key, generation, index), piece))
            .and_then(|()| {
                slots.set(
                    key,
                    &Manifest {
                        generation: generation.to_owned(),
                        count: pieces.len(),
                        units,
                        checksum: checksum(value),
                    }
                    .render(),
                )
            });
        if let Err(e) = committed {
            // The previous value still stands; drop what this write added.
            // If cleanup fails too, the pending record lets the next change finish it.
            if delete_chunks(slots, key, generation, pieces.len()).is_ok() {
                // Only the still-live previous generation was recorded besides ours.
                let _ = slots.delete(&record);
            }
            return Err(ChunkError::Store(e));
        }
    } else {
        slots.set(key, value).map_err(ChunkError::Store)?;
    }
    if let Some(previous) = previous {
        // The key no longer names these chunks. A failure here keeps the
        // pending record, so the next change of this key removes them.
        if delete_chunks(slots, key, &previous.generation, previous.count).is_err() {
            return Ok(());
        }
    }
    if !pending.is_empty() {
        let _ = slots.delete(&record);
    }
    Ok(())
}

/// Deletes `key` and, when chunked, its chunks (the manifest first, so no
/// reader ever sees a manifest without its chunks).
pub(crate) fn delete<S: Slots>(slots: &S, key: &str) -> Result<(), ChunkError<S::Error>> {
    let _guard = slots.exclusive(key).map_err(ChunkError::Store)?;
    recover(slots, key)?;
    let previous = current_manifest(slots, key)?;
    let record = pending_key(key);
    if let Some(previous) = &previous {
        slots
            .set(
                &record,
                &render_pending(&[(previous.generation.clone(), previous.count)]),
            )
            .map_err(ChunkError::Store)?;
    }
    slots.delete(key).map_err(ChunkError::Store)?;
    if let Some(previous) = previous {
        delete_chunks(slots, key, &previous.generation, previous.count)?;
        slots.delete(&record).map_err(ChunkError::Store)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};

    /// Credential Manager's limit, a per-key lock and injected failures:
    /// `fail_on_set` fails one write; `crash_after` fails every operation
    /// from that index on, as a writer process killed mid-change.
    #[derive(Default)]
    struct FakeSlots {
        entries: Mutex<BTreeMap<String, String>>,
        fail_on_set: Mutex<Option<usize>>,
        crash_after: Mutex<Option<usize>>,
        sets: AtomicUsize,
        operations: AtomicUsize,
        locks: Arc<(Mutex<Vec<String>>, Condvar)>,
    }
    struct FakeGuard {
        key: String,
        locks: Arc<(Mutex<Vec<String>>, Condvar)>,
    }
    impl Drop for FakeGuard {
        fn drop(&mut self) {
            let (held, released) = &*self.locks;
            held.lock().unwrap().retain(|k| *k != self.key);
            released.notify_all();
        }
    }
    impl FakeSlots {
        fn operate(&self) -> Result<(), String> {
            let n = self.operations.fetch_add(1, Ordering::SeqCst);
            match *self.crash_after.lock().unwrap() {
                Some(limit) if n >= limit => Err("synthetic crash".into()),
                _ => Ok(()),
            }
        }
        fn keys(&self) -> Vec<String> {
            self.entries.lock().unwrap().keys().cloned().collect()
        }
        fn restart(&self) {
            *self.crash_after.lock().unwrap() = None;
        }
    }
    impl Slots for FakeSlots {
        type Error = String;
        type Guard = FakeGuard;
        fn get(&self, key: &str) -> Result<Option<String>, String> {
            self.operate()?;
            Ok(self.entries.lock().unwrap().get(key).cloned())
        }
        fn set(&self, key: &str, value: &str) -> Result<(), String> {
            self.operate()?;
            let n = self.sets.fetch_add(1, Ordering::SeqCst);
            if *self.fail_on_set.lock().unwrap() == Some(n) {
                return Err("synthetic write failure".into());
            }
            if value.encode_utf16().count() * 2 > 2560 {
                return Err("password too long".into());
            }
            self.entries
                .lock()
                .unwrap()
                .insert(key.into(), value.into());
            Ok(())
        }
        fn delete(&self, key: &str) -> Result<(), String> {
            self.operate()?;
            self.entries.lock().unwrap().remove(key);
            Ok(())
        }
        fn exclusive(&self, key: &str) -> Result<FakeGuard, String> {
            let (held, released) = &*self.locks;
            let mut held = held.lock().unwrap();
            while held.iter().any(|k| k == key) {
                held = released.wait(held).unwrap();
            }
            held.push(key.into());
            Ok(FakeGuard {
                key: key.into(),
                locks: Arc::clone(&self.locks),
            })
        }
    }

    fn token(len: usize) -> String {
        // Multi-unit characters straddle chunk boundaries too.
        "synthetic-token-é𝄞-".chars().cycle().take(len).collect()
    }

    #[test]
    fn large_secrets_round_trip_in_chunks_and_replace_atomically() {
        let slots = FakeSlots::default();
        let small = "synthetic-small-secret";
        write(&slots, "k", small, "g1").unwrap();
        assert_eq!(read(&slots, "k").unwrap().as_deref(), Some(small));
        assert_eq!(slots.keys(), vec!["k".to_string()]);

        let large = token(5000);
        write(&slots, "k", &large, "g2").unwrap();
        assert_eq!(read(&slots, "k").unwrap(), Some(large.clone()));
        let chunks = slots.keys().len() - 1;
        assert!(chunks >= 5, "{chunks}");

        // Replacing drops the old generation's chunks and the pending record.
        let larger = token(9000);
        write(&slots, "k", &larger, "g3").unwrap();
        assert_eq!(read(&slots, "k").unwrap(), Some(larger.clone()));
        assert!(slots.keys().iter().all(|k| !k.contains("-g2-")));
        assert!(!slots.keys().contains(&pending_key("k")));

        // A write failing midway keeps the previous value and leaves no chunks.
        *slots.fail_on_set.lock().unwrap() = Some(slots.sets.load(Ordering::SeqCst) + 3);
        assert!(write(&slots, "k", &token(7000), "g4").is_err());
        assert_eq!(read(&slots, "k").unwrap(), Some(larger));
        assert!(slots.keys().iter().all(|k| !k.contains("-g4-")));

        // A small value replacing a chunked one cleans up too.
        write(&slots, "k", small, "g5").unwrap();
        assert_eq!(slots.keys(), vec!["k".to_string()]);

        delete(&slots, "k").unwrap();
        assert!(slots.keys().is_empty());
        assert_eq!(read(&slots, "k").unwrap(), None);
    }

    #[test]
    fn a_writer_killed_at_any_step_leaves_a_whole_value_and_no_lasting_orphans() {
        let old = token(4000);
        let new = token(6000);
        let mut step = 0;
        loop {
            let slots = FakeSlots::default();
            write(&slots, "k", &old, "gold").unwrap();
            write(&slots, "other", &token(3000), "gother").unwrap();
            let before = slots.operations.load(Ordering::SeqCst);
            *slots.crash_after.lock().unwrap() = Some(before + step);
            let crash_at = before + step;
            let failed = write(&slots, "k", &new, "gnew").is_err();
            // The write ran to its end without reaching the crash point.
            let finished = !failed && slots.operations.load(Ordering::SeqCst) <= crash_at;
            slots.restart();
            // Whatever the crash point, readers see exactly one whole value.
            let seen = read(&slots, "k").unwrap().unwrap();
            assert!(seen == old || seen == new, "torn value at step {step}");
            // The next change of the key removes every leftover of the crash,
            // and never another key's entries.
            write(&slots, "k", "synthetic-small", "gnext").unwrap();
            let mut keys = slots.keys();
            keys.retain(|k| !k.starts_with("other"));
            assert_eq!(keys, vec!["k".to_string()], "orphans after step {step}");
            assert_eq!(
                read(&slots, "other").unwrap().unwrap(),
                token(3000),
                "another key was touched at step {step}"
            );
            if finished {
                break;
            }
            step += 1;
        }
        assert!(step > 5, "the crash sweep covered too few steps");
    }

    #[test]
    fn a_crashed_delete_is_finished_by_the_next_change() {
        let slots = FakeSlots::default();
        write(&slots, "k", &token(5000), "g1").unwrap();
        let before = slots.operations.load(Ordering::SeqCst);
        // Crash right after the key itself is deleted, before its chunks.
        *slots.crash_after.lock().unwrap() = Some(before + 5);
        assert!(delete(&slots, "k").is_err());
        slots.restart();
        assert_eq!(read(&slots, "k").unwrap(), None);
        delete(&slots, "k").unwrap();
        assert!(slots.keys().is_empty(), "{:?}", slots.keys());
    }

    #[test]
    fn concurrent_writers_of_one_key_are_serialized() {
        let slots = Arc::new(FakeSlots::default());
        let writers: Vec<_> = (0..4)
            .map(|n| {
                let slots = Arc::clone(&slots);
                std::thread::spawn(move || {
                    for round in 0..5 {
                        let value = format!("{n}-{round}-{}", token(3000 + n * 100));
                        write(&*slots, "k", &value, &format!("g{n}r{round}")).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let value = read(&*slots, "k").unwrap().unwrap();
        let manifest = current_manifest(&*slots, "k").unwrap().unwrap();
        assert!(
            (0..4)
                .any(|n| (0..5)
                    .any(|round| value == format!("{n}-{round}-{}", token(3000 + n * 100)))),
            "the stored value is not one writer's whole value"
        );
        // Exactly the live generation's chunks remain.
        let mut expected: Vec<String> = (0..manifest.count)
            .map(|i| chunk_key("k", &manifest.generation, i))
            .collect();
        expected.push("k".into());
        expected.sort();
        assert_eq!(slots.keys(), expected);
    }

    #[test]
    fn manifest_lookalikes_and_damage_are_handled() {
        let slots = FakeSlots::default();
        let lookalike = format!("{MANIFEST}not-a-real-manifest");
        write(&slots, "k", &lookalike, "g1").unwrap();
        assert_eq!(read(&slots, "k").unwrap(), Some(lookalike));

        write(&slots, "k", &token(3000), "g2").unwrap();
        slots
            .entries
            .lock()
            .unwrap()
            .insert(chunk_key("k", "g2", 1), "tampered".into());
        assert!(matches!(read(&slots, "k"), Err(ChunkError::Corrupt(_))));
        assert!(write(&slots, "k", "x", "bad-generation").is_err());
        // A malformed pending record names nothing to delete.
        assert_eq!(parse_pending(&format!("{PENDING}g1:2,bad-gen:1")), None);
        assert_eq!(parse_pending(&format!("{PENDING}g1:999999")), None);
        assert_eq!(
            parse_pending(&render_pending(&[("g1".into(), 2), ("g2".into(), 3)])),
            Some(vec![("g1".into(), 2), ("g2".into(), 3)])
        );
    }

    #[test]
    fn pieces_respect_the_unit_limit_and_character_boundaries() {
        let value = token(4321);
        let pieces = split(&value, MAX_UNITS);
        assert!(pieces.iter().all(|p| p.encode_utf16().count() <= MAX_UNITS));
        assert_eq!(pieces.concat(), value);
        assert!(split("", MAX_UNITS).is_empty());
    }
}
