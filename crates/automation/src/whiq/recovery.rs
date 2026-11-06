//! The open documents as the worker last published them, so unsaved work is written to its
//! recovered file without waiting for the worker: the checkpoint timer (`--checkpoint-secs`),
//! SIGTERM and hang-up save from here while a long call (OCR, a big save) still runs. The
//! worker publishes after every call and after every page OCR adds.
//!
//! Every recovered file is written through [`Recovery::write`], one at a time: a document keeps
//! one file for its whole unsaved stretch (each write replaces it atomically), and a state is
//! never written over a later one (documents count their states by generation).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// One open document as published.
#[derive(Clone, Debug)]
pub struct DocSnap {
    pub id: u64,
    pub generation: u64,
    /// The working file, when the document has unsaved work (`None`: nothing to save).
    pub bytes: Option<Arc<Vec<u8>>>,
    /// The folder a new recovered file goes in (resolved inside the write root) and its stem.
    pub dir: PathBuf,
    pub stem: String,
    /// The original file, for the report.
    pub original: Option<String>,
    /// When unsaved work not yet in a recovered file was first published (the timer's clock).
    pub since: Option<Instant>,
    /// Calls that changed the document since its last checkpoint.
    pub edits: u32,
    /// The generation the last call left (edits are counted per call).
    pub seen: u64,
}

/// A document's recovered file and the generation written there (`path` is `None` once the
/// document was saved and its checkpoint removed).
#[derive(Clone, Debug)]
struct Written {
    path: Option<PathBuf>,
    generation: u64,
}

#[derive(Default)]
struct State {
    docs: Vec<DocSnap>,
    files: HashMap<u64, Written>,
    stop: bool,
}

#[derive(Default)]
pub struct Recovery {
    state: Mutex<State>,
    /// Wakes the checkpoint timer when the worker publishes or the server stops.
    cv: Condvar,
    /// One recovered-file write (or removal) at a time.
    writing: Mutex<()>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Recovery {
    pub fn new() -> Arc<Recovery> {
        Arc::new(Recovery::default())
    }

    /// The worker's view of the open documents, replacing the last one (a document no longer
    /// open is dropped; its recovered file stays where it is). Each entry's edit count carries
    /// over; the state a document is first published in (as opened) needs no checkpoint.
    pub fn publish(&self, docs: Vec<DocSnap>) {
        let mut st = lock(&self.state);
        let now = Instant::now();
        let mut docs = docs;
        for d in &mut docs {
            let prev = st.docs.iter().find(|p| p.id == d.id).cloned();
            match &prev {
                Some(p) => {
                    d.edits = p.edits;
                    d.seen = p.seen;
                }
                None => d.seen = d.generation,
            }
            let written = st.files.entry(d.id).or_insert(Written { path: None, generation: d.generation }).generation;
            // Unsaved work newer than its recovered file starts (or keeps) the timer's clock.
            d.since = (d.bytes.is_some() && d.generation > written).then(|| prev.and_then(|p| p.since).unwrap_or(now));
        }
        st.docs = docs;
        drop(st);
        self.cv.notify_all();
    }

    /// The published entry of document `id`.
    pub fn doc(&self, id: u64) -> Option<DocSnap> {
        lock(&self.state).docs.iter().find(|d| d.id == id).cloned()
    }

    /// Count a call that changed document `id` (at `generation`), and say how many edits are
    /// now unsaved in no recovered file.
    pub fn count_edit(&self, id: u64, generation: u64) -> u32 {
        let mut st = lock(&self.state);
        let Some(d) = st.docs.iter_mut().find(|d| d.id == id) else { return 0 };
        if d.seen != generation {
            d.seen = generation;
            if d.bytes.is_some() {
                d.edits = d.edits.saturating_add(1);
            }
        }
        if d.bytes.is_none() {
            d.edits = 0;
        }
        d.edits
    }

    /// The recovered file of document `id`, if it has one.
    pub fn file_of(&self, id: u64) -> Option<PathBuf> {
        lock(&self.state).files.get(&id).and_then(|w| w.path.clone())
    }

    /// Every recovered file of an open document with the edits not yet in it.
    pub fn files(&self) -> Vec<(u64, PathBuf, u32)> {
        let st = lock(&self.state);
        let mut out: Vec<(u64, PathBuf, u32)> = st
            .files
            .iter()
            .filter_map(|(id, w)| w.path.clone().map(|p| (*id, p, st.docs.iter().find(|d| d.id == *id).map_or(0, |d| d.edits))))
            .collect();
        out.sort_by_key(|f| f.0);
        out
    }

    /// Write `bytes`, document `id` at `generation`, to its recovered file (a new
    /// `<stem>.recovered-<stamp>.pdf` in `dir` when it has none). A state the file holds already
    /// is not written again, and one older than a save is not written at all. `published`: only
    /// while the document is still published (the timer and the signal thread, so a document
    /// closed or released meanwhile is left alone). The file holding the state, if any.
    pub fn write(&self, id: u64, generation: u64, bytes: &[u8], dir: &Path, stem: &str, published: bool) -> Result<Option<PathBuf>, String> {
        let _one = lock(&self.writing);
        let mut st = lock(&self.state);
        let written = st.files.get(&id).cloned();
        if let Some(w) = &written {
            let held = w.path.is_some() && w.generation >= generation;
            if held || w.generation > generation {
                if let Some(d) = st.docs.iter_mut().find(|d| d.id == id && d.generation <= w.generation) {
                    d.since = None;
                }
                return Ok(w.path.clone().filter(|_| held));
            }
        }
        if published && !st.docs.iter().any(|d| d.id == id) {
            return Ok(None);
        }
        drop(st);
        let target = match written.and_then(|w| w.path) {
            Some(p) => p,
            None => new_name(dir, stem),
        };
        crate::paths::write_atomic(&target, bytes).map_err(|e| e.to_string())?;
        let mut st = lock(&self.state);
        st.files.insert(id, Written { path: Some(target.clone()), generation });
        if let Some(d) = st.docs.iter_mut().find(|d| d.id == id) {
            d.edits = 0;
            d.since = (d.generation > generation).then(Instant::now);
        }
        Ok(Some(target))
    }

    /// Write a published state (see [`Self::write`]), while its document is still published.
    pub fn write_snapshot(&self, d: &DocSnap) -> Result<Option<PathBuf>, String> {
        match &d.bytes {
            Some(b) => self.write(d.id, d.generation, b, &d.dir, &d.stem, true),
            None => Ok(None),
        }
    }

    /// Document `id` was saved (at `generation`): its checkpoint file goes, and no older state
    /// is written after it.
    pub fn saved(&self, id: u64, generation: u64) {
        let _one = lock(&self.writing);
        let mut st = lock(&self.state);
        let old = st.files.insert(id, Written { path: None, generation });
        if let Some(p) = old.and_then(|w| w.path) {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Document `id` was closed or released: it is no longer saved from here. `remove`: its
    /// checkpoint file goes too (closed with its changes discarded).
    pub fn forget(&self, id: u64, remove: bool) {
        let _one = lock(&self.writing);
        let mut st = lock(&self.state);
        st.docs.retain(|d| d.id != id);
        if let Some(p) = st.files.remove(&id).and_then(|w| w.path)
            && remove
        {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Save every published document with unsaved work now (SIGTERM, hang-up): the files
    /// written, with the original each came from.
    pub fn save_all(&self, why: &str) -> Vec<(PathBuf, Option<String>)> {
        let docs: Vec<DocSnap> = lock(&self.state).docs.iter().filter(|d| d.bytes.is_some()).cloned().collect();
        let mut out = Vec::new();
        for d in docs {
            match self.write_snapshot(&d) {
                Ok(Some(p)) => {
                    eprintln!("pdfcraft-cli: unsaved work auto-saved ({why}) at {}", p.display());
                    out.push((p, d.original.clone()));
                }
                Ok(None) => {}
                Err(e) => eprintln!("pdfcraft-cli: could not auto-save {} ({why}): {e}", d.stem),
            }
        }
        out
    }

    /// Stop [`Self::run_timer`].
    pub fn stop(&self) {
        lock(&self.state).stop = true;
        self.cv.notify_all();
    }

    /// The checkpoint timer: a document whose unsaved work has waited `every` without reaching a
    /// recovered file is checkpointed from its published state, whatever call is running. Sleeps
    /// until the next one is due (or the worker publishes). Returns on [`Self::stop`].
    pub fn run_timer(&self, every: Duration) {
        let mut st = lock(&self.state);
        loop {
            if st.stop {
                return;
            }
            let now = Instant::now();
            let due = st.docs.iter().filter(|d| d.bytes.is_some()).filter_map(|d| d.since.map(|s| (s + every, d.id))).min_by_key(|x| x.0);
            match due {
                None => st = self.cv.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner),
                Some((at, _)) if at > now => {
                    st = self.cv.wait_timeout(st, at - now).map_or_else(|e| e.into_inner().0, |r| r.0);
                }
                Some((_, id)) => {
                    let Some(snap) = st.docs.iter().find(|d| d.id == id).cloned() else { continue };
                    drop(st);
                    let wrote = self.write_snapshot(&snap);
                    st = lock(&self.state);
                    if !matches!(wrote, Ok(Some(_))) {
                        if let Err(e) = &wrote {
                            eprintln!("pdfcraft-cli: could not write a checkpoint of {}: {e}", snap.stem);
                        }
                        // Not written: tried again after another interval, not at once.
                        if let Some(d) = st.docs.iter_mut().find(|d| d.id == id && d.since == snap.since) {
                            d.since = Some(Instant::now());
                        }
                    }
                }
            }
        }
    }
}

/// A recovered file name not taken yet: `<stem>.recovered-<stamp>.pdf`, else `-2`, `-3`…
fn new_name(dir: &Path, stem: &str) -> PathBuf {
    let base = format!("{stem}.recovered-{}", super::engine::stamp());
    let mut cand = dir.join(format!("{base}.pdf"));
    let mut n = 2u32;
    while cand.exists() && n < 1000 {
        cand = dir.join(format!("{base}-{n}.pdf"));
        n += 1;
    }
    cand
}
