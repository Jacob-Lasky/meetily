//! What Live Assist keeps with the recording (WORK-672): every card it
//! showed, and the after-call output.
//!
//! - `live-assist-cards.jsonl`: one JSON event per line, appended as it
//!   happens, so a crash or a killed app keeps everything shown up to then.
//!   A card is logged with the transcript line its quote maps to and the
//!   span of transcript Live Assist was following when it arrived; later
//!   events record a card taken back (its line was retracted as bleed),
//!   pushed off the panel, dismissed, or pinned.
//! - `live-assist-after-call.json`: one more call once the recording stops,
//!   over the full transcript plus the meeting brief (`cards::after_call_*`).
//!
//! Card text is built from the meeting, like the transcript: these files
//! stay in the meeting folder, and DO NOT log their contents.
//!
//! Plain files, deliberately no DB table or column: a migration would stop
//! the previous AppImage opening the DB (see `mod.rs`).

use std::io::Write;
use std::path::{Path, PathBuf};

use log::{error, warn};
use serde::{Deserialize, Serialize};

use super::cards::Side;
use super::engine::Card;

pub const CARDS_FILE: &str = "live-assist-cards.jsonl";
pub const AFTER_CALL_FILE: &str = "live-assist-after-call.json";

/// One transcript line, as the record cites it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpanLine {
    pub seq: u64,
    /// "me" | "them" | "unknown".
    pub side: String,
    pub text: String,
    /// Seconds from recording start (the transcript's `audio_start_time`).
    pub start_s: f64,
    pub end_s: f64,
}

/// The transcript Live Assist was following when a card arrived: the
/// sequence ids and recording times of its first and last line (they match
/// `sequence_id` / `audio_start_time` in `transcripts.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub from_seq: u64,
    pub to_seq: u64,
    pub from_s: f64,
    pub to_s: f64,
    pub lines: usize,
}

impl Span {
    /// The span of `lines` (oldest first); None when there are none.
    pub fn of(lines: &[&SpanLine]) -> Option<Span> {
        let first = lines.first()?;
        let last = lines.last()?;
        Some(Span {
            from_seq: first.seq,
            to_seq: last.seq,
            from_s: lines.iter().map(|l| l.start_s).fold(f64::INFINITY, f64::min),
            to_s: lines.iter().map(|l| l.end_s).fold(f64::NEG_INFINITY, f64::max),
            lines: lines.len(),
        })
    }
}

pub fn side_name(side: Side) -> &'static str {
    match side {
        Side::Me => "me",
        Side::Them => "them",
        Side::Unknown => "unknown",
    }
}

/// The inverse of `side_name`.
pub fn side_from_name(name: &str) -> Side {
    match name {
        "me" => Side::Me,
        "them" => Side::Them,
        _ => Side::Unknown,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CardEvent {
    /// A card reached the panel. `source`: the line its quote maps to (none
    /// for cards read from the brief); `window`: what Live Assist was
    /// following when it arrived.
    Shown {
        at_ms: i64,
        card: Card,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<SpanLine>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<Span>,
    },
    /// Taken off the panel: "retracted" (its line was Mic-In bleed and left
    /// the transcript) or "evicted" (pushed out by newer cards, or a cleared
    /// brief). Either way the card was shown and stays in the record.
    Withdrawn { at_ms: i64, ids: Vec<String>, reason: String },
    Dismissed { at_ms: i64, id: String },
    Pinned { at_ms: i64, id: String, pinned: bool },
}

/// Appends card events to the meeting folder's `CARDS_FILE`. Without a
/// folder it keeps nothing (and says so once); a failed write is logged once
/// per run of failures, and the next event tries again.
pub struct CardLog {
    path: Option<PathBuf>,
    warned_no_folder: bool,
    failing: bool,
}

impl CardLog {
    pub fn new(folder: Option<&Path>) -> Self {
        Self { path: folder.map(|f| f.join(CARDS_FILE)), warned_no_folder: false, failing: false }
    }

    pub fn append(&mut self, event: &CardEvent) {
        let Some(path) = &self.path else {
            if !self.warned_no_folder {
                warn!("live assist: this recording has no meeting folder; its cards will not be kept");
                self.warned_no_folder = true;
            }
            return;
        };
        match append_line(path, event) {
            Ok(()) => self.failing = false,
            Err(e) => {
                if !self.failing {
                    error!("live assist: could not record cards in {}: {e}", path.display());
                }
                self.failing = true;
            }
        }
    }
}

fn append_line(path: &Path, event: &CardEvent) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    // One write per event: an O_APPEND write of one line is not interleaved.
    file.write_all(&line)?;
    file.flush()
}

/// A card from the record with what happened to it after it was shown.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordedCard {
    pub card: Card,
    pub shown_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SpanLine>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<Span>,
    /// Set when it was taken back because its transcript line was retracted.
    pub retracted: bool,
    pub dismissed: bool,
    /// The last pin state the record holds.
    pub pinned: bool,
}

/// Every card the record holds, in the order shown. A missing file is an
/// empty record. Unreadable lines are skipped (a crash mid-write leaves at
/// most a partial last line), and counted in the second value.
pub fn read_cards(folder: &Path) -> Result<(Vec<RecordedCard>, usize), String> {
    let path = folder.join(CARDS_FILE);
    let body = match std::fs::read(&path) {
        // Lossy: a write cut off inside a multi-byte character must cost
        // that one line (it then fails to parse), not the whole record.
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    let mut cards: Vec<RecordedCard> = Vec::new();
    let mut skipped = 0;
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(event) = serde_json::from_str::<CardEvent>(line) else {
            skipped += 1;
            continue;
        };
        match event {
            CardEvent::Shown { at_ms, card, source, window } => {
                let pinned = card.pinned;
                cards.push(RecordedCard {
                    card,
                    shown_at_ms: at_ms,
                    source,
                    window,
                    retracted: false,
                    dismissed: false,
                    pinned,
                });
            }
            CardEvent::Withdrawn { ids, reason, .. } => {
                if reason == "retracted" {
                    for c in cards.iter_mut().filter(|c| ids.contains(&c.card.id)) {
                        c.retracted = true;
                    }
                }
            }
            CardEvent::Dismissed { id, .. } => {
                for c in cards.iter_mut().filter(|c| c.card.id == id) {
                    c.dismissed = true;
                }
            }
            CardEvent::Pinned { id, pinned, .. } => {
                for c in cards.iter_mut().filter(|c| c.card.id == id) {
                    c.pinned = pinned;
                }
            }
        }
    }
    Ok((cards, skipped))
}

// ============================================================================
// AFTER-CALL OUTPUT
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfterCallStatus {
    Done,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AfterCall {
    pub version: u32,
    pub status: AfterCallStatus,
    pub created_at_ms: i64,
    pub provider: String,
    pub model: String,
    /// Markdown, as the model wrote it. Empty on an error.
    #[serde(default)]
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Transcript lines the call read, and whether the oldest were left out
    /// to fit (`cards::AFTER_CALL_MAX_TRANSCRIPT_CHARS`).
    pub transcript_lines: usize,
    #[serde(default)]
    pub transcript_clipped: bool,
    pub had_brief: bool,
    pub call_ms: u64,
}

/// Write atomically: temp file and rename in the same folder.
pub fn write_after_call(folder: &Path, after: &AfterCall) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(after)?;
    let tmp = folder.join(format!(".{AFTER_CALL_FILE}.tmp"));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, folder.join(AFTER_CALL_FILE))
}

pub fn read_after_call(folder: &Path) -> Result<Option<AfterCall>, String> {
    let path = folder.join(AFTER_CALL_FILE);
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    serde_json::from_slice(&body).map(Some).map_err(|e| format!("could not parse {}: {e}", path.display()))
}

/// The saved transcript of a meeting folder (`transcripts.json`, which
/// never keeps a retracted line), in order of each line's START time.
///
/// DO NOT use the file order for the conversation: it is the order the
/// finals ARRIVED, and a long turn on one side is finalised only when it
/// ends, so it lands after the other side's short replies made during it.
/// Measured on WORK-672's 45-minute interview: 51 of 289 lines started more
/// than 3 s (up to 111 s) before the line above them in the file, while each
/// channel's own times were monotonic and both spanned the whole recording
/// (no clock offset between the channels). Time order is the conversation.
pub fn read_transcript(folder: &Path) -> Result<Vec<SpanLine>, String> {
    #[derive(Deserialize)]
    struct Segment {
        text: String,
        #[serde(default)]
        audio_start_time: f64,
        #[serde(default)]
        audio_end_time: f64,
        #[serde(default)]
        sequence_id: u64,
        #[serde(default)]
        speaker: Option<String>,
    }
    #[derive(Deserialize)]
    struct File {
        segments: Vec<Segment>,
    }
    let path = folder.join("transcripts.json");
    let body = std::fs::read(&path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let file: File = serde_json::from_slice(&body).map_err(|e| format!("could not parse {}: {e}", path.display()))?;
    let mut lines: Vec<SpanLine> = file
        .segments
        .into_iter()
        .filter(|s| !s.text.trim().is_empty())
        .map(|s| SpanLine {
            seq: s.sequence_id,
            side: side_name(Side::from_speaker(s.speaker.as_deref())).to_string(),
            text: s.text,
            start_s: s.audio_start_time,
            end_s: s.audio_end_time,
        })
        .collect();
    // Stable: lines with equal start times keep their arrival order.
    lines.sort_by(|a, b| a.start_s.total_cmp(&b.start_s));
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::super::cards::CardKind;
    use super::*;

    fn card(id: &str) -> Card {
        Card {
            id: id.into(),
            kind: CardKind::Ask,
            text: "Ask how they size GPUs".into(),
            quote: "we run our own GPUs".into(),
            source_seq: 4,
            pinned: false,
            created_at_ms: 1_763_500_000_000,
            origin: None,
        }
    }

    fn line(seq: u64, start: f64) -> SpanLine {
        SpanLine { seq, side: "them".into(), text: format!("line {seq}"), start_s: start, end_s: start + 2.0 }
    }

    #[test]
    fn every_event_is_appended_and_folded_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = CardLog::new(Some(dir.path()));
        let l3 = line(3, 10.0);
        let l4 = line(4, 13.0);
        let window = Span::of(&[&l3, &l4]);
        log.append(&CardEvent::Shown { at_ms: 1, card: card("a"), source: Some(l4.clone()), window: window.clone() });
        log.append(&CardEvent::Shown { at_ms: 2, card: card("b"), source: None, window: None });
        log.append(&CardEvent::Shown { at_ms: 3, card: card("c"), source: None, window: None });
        log.append(&CardEvent::Pinned { at_ms: 4, id: "a".into(), pinned: true });
        log.append(&CardEvent::Withdrawn { at_ms: 5, ids: vec!["b".into()], reason: "retracted".into() });
        log.append(&CardEvent::Withdrawn { at_ms: 6, ids: vec!["c".into()], reason: "evicted".into() });
        log.append(&CardEvent::Dismissed { at_ms: 7, id: "c".into() });

        let (cards, skipped) = read_cards(dir.path()).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(cards.iter().map(|c| c.card.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert!(cards[0].pinned && !cards[0].retracted);
        assert_eq!(cards[0].source.as_ref(), Some(&l4));
        assert_eq!(
            cards[0].window,
            Some(Span { from_seq: 3, to_seq: 4, from_s: 10.0, to_s: 15.0, lines: 2 })
        );
        assert!(cards[1].retracted, "a retracted card stays in the record, marked");
        assert!(!cards[2].retracted && cards[2].dismissed, "eviction is not a retraction");
    }

    #[test]
    fn a_partial_last_line_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = CardLog::new(Some(dir.path()));
        log.append(&CardEvent::Shown { at_ms: 1, card: card("a"), source: None, window: None });
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.path().join(CARDS_FILE)).unwrap();
        // Cut off inside a multi-byte character (the first byte of "é").
        f.write_all(b"{\"event\":\"shown\",\"at_ms\":2,\"text\":\"caf\xc3").unwrap();
        let (cards, skipped) = read_cards(dir.path()).unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(skipped, 1);
    }

    #[test]
    fn no_file_is_an_empty_record_and_no_folder_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_cards(dir.path()).unwrap(), (Vec::new(), 0));
        let mut log = CardLog::new(None);
        log.append(&CardEvent::Dismissed { at_ms: 1, id: "a".into() });
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        assert_eq!(read_after_call(dir.path()), Ok(None));
    }

    #[test]
    fn after_call_round_trips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let after = AfterCall {
            version: 1,
            status: AfterCallStatus::Done,
            created_at_ms: 9,
            provider: "claude".into(),
            model: "m".into(),
            output: "## Signals\n- one".into(),
            error: None,
            transcript_lines: 2,
            transcript_clipped: false,
            had_brief: true,
            call_ms: 1200,
        };
        write_after_call(dir.path(), &after).unwrap();
        assert_eq!(read_after_call(dir.path()).unwrap(), Some(after));
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from(AFTER_CALL_FILE)], "no temp file left behind");
    }

    #[test]
    fn the_saved_transcript_is_read_in_start_time_order_with_sides() {
        let dir = tempfile::tempdir().unwrap();
        let body = serde_json::json!({
            "version": "1.0",
            "segments": [
                {"id": "x", "text": "hello there", "audio_start_time": 5.0, "audio_end_time": 6.0, "duration": 1.0,
                 "display_time": "[00:05]", "confidence": 0.9, "sequence_id": 9, "speaker": "Speaker-Out"},
                {"id": "y", "text": "  ", "audio_start_time": 6.0, "audio_end_time": 6.5, "duration": 0.5,
                 "display_time": "[00:06]", "confidence": 0.9, "sequence_id": 10, "speaker": "Mic-In"},
                {"id": "z", "text": "hi", "audio_start_time": 1.0, "audio_end_time": 2.0, "duration": 1.0,
                 "display_time": "[00:01]", "confidence": 0.9, "sequence_id": 11, "speaker": "Mic-In"}
            ],
            "total_segments": 3
        });
        std::fs::write(dir.path().join("transcripts.json"), body.to_string()).unwrap();
        let lines = read_transcript(dir.path()).unwrap();
        // Line 11 arrived last but started first (a turn finalised late).
        assert_eq!(lines.iter().map(|l| (l.seq, l.side.as_str())).collect::<Vec<_>>(), [(11, "me"), (9, "them")]);
    }
}
