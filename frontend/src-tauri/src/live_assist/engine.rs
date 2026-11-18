//! Live-assist state machine: the context window, the trigger and debounce,
//! retraction, and the card list. Pure: the caller passes `now`, so tests
//! drive it with a fake clock. The Tauri glue lives in `mod.rs`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::cards::{normalize, CardKind, RawCard, Side};

#[derive(Debug, Clone)]
pub struct Config {
    /// Wait this long after the last Speaker-Out final before calling, so a
    /// turn that arrives as several finals becomes one call.
    pub settle: Duration,
    /// At most one call starts per this interval.
    pub min_interval: Duration,
    /// Context window: at most this many lines...
    pub max_lines: usize,
    /// ...and none older than this.
    pub max_age: Duration,
    /// Unpinned cards kept; older ones are evicted (and withdrawn in the UI).
    pub max_unpinned_cards: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            settle: Duration::from_millis(1500),
            min_interval: Duration::from_secs(5),
            max_lines: 12,
            max_age: Duration::from_secs(120),
            max_unpinned_cards: 20,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Line {
    pub seq: u64,
    pub side: Side,
    pub text: String,
    pub at: Instant,
}

/// A card shown in the panel. Serialized as the `assist-update` payload items
/// and as the saved pins (`live-assist.json`); field names are the contract
/// with `frontend/src/lib/live-assist.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct Card {
    pub id: String,
    pub kind: CardKind,
    pub text: String,
    pub quote: String,
    /// `sequence_id` of the transcript line the quote maps to. A
    /// `transcript-retract` of that line withdraws the card.
    pub source_seq: u64,
    pub pinned: bool,
    /// Unix epoch milliseconds.
    pub created_at_ms: i64,
    /// Where a research card's fact came from (the MCP servers the lookup
    /// used, WORK-581). Absent on cards from the transcript and background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// What a call needs: its id (to match the result), the context lines and
/// the texts of the cards already on screen, newest first.
#[derive(Debug, Clone)]
pub struct CallRequest {
    pub call_id: u64,
    pub lines: Vec<Line>,
    pub shown: Vec<String>,
}

/// Most shown-card texts sent with a call; older cards have scrolled away.
const MAX_SHOWN_IN_PROMPT: usize = 10;

#[derive(Debug, Default)]
pub struct Engine {
    cfg: Config,
    lines: VecDeque<Line>,
    /// When the pending trigger may fire (None: nothing pending).
    due: Option<Instant>,
    /// The call in flight, with the context it was sent.
    in_flight: Option<CallRequest>,
    last_call: Option<Instant>,
    next_call_id: u64,
    next_card_no: u64,
    /// Newest first.
    cards: Vec<Card>,
    /// Lines retracted while a call that saw them was in flight.
    retracted_in_flight: Vec<u64>,
    /// Texts of cards Me dismissed, newest first (at most
    /// MAX_SHOWN_IN_PROMPT): not suggested again.
    dismissed: VecDeque<String>,
}

/// Result of applying a call's output.
#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    /// New cards, newest first.
    pub added: Vec<Card>,
    /// Ids of unpinned cards pushed out of the list.
    pub evicted: Vec<String>,
}

impl Engine {
    pub fn new(cfg: Config) -> Self {
        Self { cfg, ..Default::default() }
    }

    /// A final transcript line arrived. Speaker-Out lines (re)start the
    /// settle timer; Mic-In and unlabelled lines are context only.
    pub fn on_line(&mut self, seq: u64, side: Side, text: &str, now: Instant) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.lines.push_back(Line { seq, side, text: text.to_string(), at: now });
        self.prune(now);
        if side == Side::Them {
            self.due = Some(now + self.cfg.settle);
        }
    }

    /// A transcript line was taken back. It leaves the window and every card
    /// built from it is withdrawn (pinned ones too: the line was bleed, so
    /// the card answers something nobody said). Returns the withdrawn ids.
    pub fn on_retract(&mut self, seq: u64) -> Vec<String> {
        self.lines.retain(|l| l.seq != seq);
        if !self.lines.iter().any(|l| l.side == Side::Them) {
            self.due = None;
        }
        if self.in_flight.as_ref().is_some_and(|c| c.lines.iter().any(|l| l.seq == seq)) {
            self.retracted_in_flight.push(seq);
        }
        let (gone, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.cards).into_iter().partition(|c| c.source_seq == seq);
        self.cards = kept;
        gone.into_iter().map(|c| c.id).collect()
    }

    /// Start a call if one is due. `enabled` false drops any pending trigger,
    /// so turning the feature on later does not fire a stale one.
    pub fn poll(&mut self, now: Instant, enabled: bool) -> Option<CallRequest> {
        if !enabled {
            self.due = None;
            return None;
        }
        let due = self.due?;
        if now < due || self.in_flight.is_some() {
            return None;
        }
        if self.last_call.is_some_and(|last| now < last + self.cfg.min_interval) {
            return None;
        }
        self.prune(now);
        self.due = None;
        if !self.lines.iter().any(|l| l.side == Side::Them) {
            return None;
        }
        self.next_call_id += 1;
        let request = CallRequest {
            call_id: self.next_call_id,
            lines: self.lines.iter().cloned().collect(),
            shown: self.shown_texts(),
        };
        self.in_flight = Some(request.clone());
        self.last_call = Some(now);
        self.retracted_in_flight.clear();
        Some(request)
    }

    pub fn in_flight(&self) -> Option<u64> {
        self.in_flight.as_ref().map(|c| c.call_id)
    }

    /// The in-flight call ended without cards (error, cancel, timeout).
    pub fn on_call_failed(&mut self, call_id: u64) {
        if self.in_flight() == Some(call_id) {
            self.in_flight = None;
            self.retracted_in_flight.clear();
        }
    }

    /// Apply a call's cards. Each card is tied to the line its quote matches
    /// (falling back to the newest Speaker-Out line the call saw); cards tied
    /// to a line retracted mid-call, and repeats of a card already shown, are
    /// dropped. A result for a call that is no longer in flight is ignored.
    pub fn on_call_done(&mut self, call_id: u64, raw: Vec<RawCard>, created_at_ms: i64) -> Applied {
        // Check the id BEFORE taking: a late result for a cancelled call must
        // not clear the newer call that is in flight now.
        if self.in_flight() != Some(call_id) {
            return Applied::default();
        }
        let Some(call) = self.in_flight.take() else { return Applied::default() };
        let retracted = std::mem::take(&mut self.retracted_in_flight);
        self.add_cards(&format!("assist-{call_id}"), &call.lines, &retracted, raw, created_at_ms)
    }

    /// Apply cards from a call this engine did not start: the Pipecat
    /// sidecar's turn-triggered calls (WORK-559). Quotes are matched against
    /// the window as it is now, which already holds the turn that triggered
    /// the call (its line arrives while the model is still answering) and no
    /// longer holds lines retracted meanwhile. Same dedup and eviction rules.
    ///
    /// `origin` marks research cards (the research lane's lookups, which have
    /// their own id sequence, so their ids get their own prefix).
    pub fn on_external_cards(
        &mut self,
        call_id: u64,
        raw: Vec<RawCard>,
        origin: Option<String>,
        created_at_ms: i64,
    ) -> Applied {
        let lines: Vec<Line> = self.lines.iter().cloned().collect();
        let prefix = if origin.is_some() { format!("assist-r{call_id}") } else { format!("assist-p{call_id}") };
        let mut applied = self.add_cards(&prefix, &lines, &[], raw, created_at_ms);
        if origin.is_some() {
            let ids: Vec<String> = applied.added.iter().map(|c| c.id.clone()).collect();
            for card in self.cards.iter_mut().filter(|c| ids.contains(&c.id)) {
                card.origin = origin.clone();
            }
            for card in &mut applied.added {
                card.origin = origin.clone();
            }
        }
        applied
    }

    /// Texts sent with a call so the model does not repeat itself: the cards
    /// on screen (newest first, at most MAX_SHOWN_IN_PROMPT), then the ones
    /// Me dismissed.
    pub fn shown_texts(&self) -> Vec<String> {
        self.cards
            .iter()
            .take(MAX_SHOWN_IN_PROMPT)
            .map(|c| c.text.clone())
            .chain(self.dismissed.iter().cloned())
            .collect()
    }

    fn add_cards(
        &mut self,
        id_prefix: &str,
        lines: &[Line],
        retracted: &[u64],
        raw: Vec<RawCard>,
        created_at_ms: i64,
    ) -> Applied {
        let mut added = Vec::new();
        for card in raw {
            let Some(source_seq) = source_line(lines, &card.quote) else { continue };
            if retracted.contains(&source_seq) {
                continue;
            }
            let norm = normalize(&card.text);
            let repeat = self.cards.iter().chain(added.iter()).any(|c: &Card| normalize(&c.text) == norm)
                || self.dismissed.iter().any(|t| normalize(t) == norm);
            if repeat {
                continue;
            }
            self.next_card_no += 1;
            added.push(Card {
                id: format!("{id_prefix}-{}", self.next_card_no),
                kind: card.kind,
                text: card.text,
                quote: card.quote,
                source_seq,
                pinned: false,
                created_at_ms,
                origin: None,
            });
        }
        // Pinned cards stay on top (Jake, WORK-581: "Pin should raise the
        // question to the top"); new cards go right under them, in the
        // model's order, above the older unpinned ones.
        let (pinned, older): (Vec<Card>, Vec<Card>) = std::mem::take(&mut self.cards).into_iter().partition(|c| c.pinned);
        let mut list: Vec<Card> = pinned;
        list.extend(added.iter().cloned());
        list.extend(older);
        let mut evicted = Vec::new();
        let mut unpinned = 0;
        list.retain(|c| {
            if c.pinned {
                return true;
            }
            unpinned += 1;
            if unpinned > self.cfg.max_unpinned_cards {
                evicted.push(c.id.clone());
                return false;
            }
            true
        });
        self.cards = list;
        Applied { added, evicted }
    }

    pub fn dismiss(&mut self, id: &str) -> bool {
        let Some(pos) = self.cards.iter().position(|c| c.id == id) else {
            return false;
        };
        let card = self.cards.remove(pos);
        self.dismissed.push_front(card.text);
        self.dismissed.truncate(MAX_SHOWN_IN_PROMPT);
        true
    }

    /// Pin or unpin a card. A pinned card moves to the very top; an
    /// unpinned one to the top of the unpinned cards, under the pins.
    pub fn set_pinned(&mut self, id: &str, pinned: bool) -> Option<Card> {
        let pos = self.cards.iter().position(|c| c.id == id)?;
        let mut card = self.cards.remove(pos);
        card.pinned = pinned;
        let at = if pinned { 0 } else { self.cards.iter().take_while(|c| c.pinned).count() };
        self.cards.insert(at, card.clone());
        Some(card)
    }

    pub fn cards(&self) -> &[Card] {
        &self.cards
    }

    pub fn pinned(&self) -> Vec<Card> {
        self.cards.iter().filter(|c| c.pinned).cloned().collect()
    }

    pub fn lines(&self) -> impl Iterator<Item = &Line> {
        self.lines.iter()
    }

    fn prune(&mut self, now: Instant) {
        while self.lines.len() > self.cfg.max_lines {
            self.lines.pop_front();
        }
        while self.lines.front().is_some_and(|l| now.saturating_duration_since(l.at) > self.cfg.max_age) {
            self.lines.pop_front();
        }
    }
}

/// The line a quote came from: the newest line containing it (or contained
/// in it), else the line sharing most of its words (at least 60%), else the
/// newest Speaker-Out line. None only when the call saw no lines.
fn source_line(lines: &[Line], quote: &str) -> Option<u64> {
    let q = normalize(quote);
    if !q.is_empty() {
        if let Some(l) = lines.iter().rev().find(|l| {
            let t = normalize(&l.text);
            !t.is_empty() && (t.contains(&q) || q.contains(&t))
        }) {
            return Some(l.seq);
        }
        let q_words: Vec<&str> = q.split(' ').collect();
        let best = lines
            .iter()
            .rev()
            .map(|l| {
                let t = normalize(&l.text);
                let words: std::collections::HashSet<&str> = t.split(' ').collect();
                let shared = q_words.iter().filter(|w| words.contains(*w)).count();
                (l.seq, shared as f64 / q_words.len() as f64)
            })
            .fold(None::<(u64, f64)>, |best, cur| match best {
                Some(b) if b.1 >= cur.1 => Some(b),
                _ => Some(cur),
            });
        if let Some((seq, score)) = best {
            if score >= 0.6 {
                return Some(seq);
            }
        }
    }
    lines.iter().rev().find(|l| l.side == Side::Them).or(lines.last()).map(|l| l.seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn raw(kind: CardKind, text: &str, quote: &str) -> RawCard {
        RawCard { kind, text: text.into(), quote: quote.into() }
    }

    #[test]
    fn a_pinned_card_goes_to_the_top_and_new_cards_come_in_under_the_pins() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "What does streaming cost per minute?", t0);
        let first = e.on_external_cards(1, vec![raw(CardKind::Ask, "Which volume?", ""), raw(CardKind::Todo, "Send pricing", "")], None, 1);
        let ids: Vec<String> = first.added.iter().map(|c| c.id.clone()).collect();
        assert_eq!(e.cards().iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), ["Which volume?", "Send pricing"]);
        e.set_pinned(&ids[1], true).unwrap();
        assert_eq!(e.cards()[0].text, "Send pricing", "pinning raises the card to the top");
        e.on_external_cards(2, vec![raw(CardKind::Fact, "Nova-3 $0.0092/min", "")], None, 2);
        let order: Vec<&str> = e.cards().iter().map(|c| c.text.as_str()).collect();
        assert_eq!(order, ["Send pricing", "Nova-3 $0.0092/min", "Which volume?"], "new cards under the pins");
        e.set_pinned(&ids[1], false).unwrap();
        assert_eq!(e.cards()[0].text, "Send pricing", "unpinned, it heads the unpinned cards");
        assert!(!e.cards()[0].pinned);
    }

    // ---- cards from the Pipecat sidecar (WORK-559) -----------------------

    #[test]
    fn sidecar_cards_map_to_the_current_window_and_dedup() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "We run forty thousand calls a day.", t0);
        e.on_line(2, Side::Me, "Got it.", t0);
        // The triggering turn's line lands while the sidecar's model answers.
        e.on_line(3, Side::Them, "What does pricing look like?", t0);
        let applied = e.on_external_cards(
            7,
            vec![
                raw(CardKind::Ask, "Which plan?", "pricing look like"),
                raw(CardKind::Fact, "40k calls a day", "forty thousand calls"),
                raw(CardKind::Ask, "Which plan?", "pricing"),
            ],
            None,
            5,
        );
        let got: Vec<(&str, u64)> = applied.added.iter().map(|c| (c.text.as_str(), c.source_seq)).collect();
        assert_eq!(got, vec![("Which plan?", 3), ("40k calls a day", 1)], "repeat dropped");
        assert!(applied.added[0].id.starts_with("assist-p7-"), "sidecar ids cannot collide with timer ids");
        assert_eq!(e.in_flight(), None, "no Rust call is involved");
        e.dismiss(&applied.added[1].id.clone());
        assert_eq!(e.shown_texts(), vec!["Which plan?".to_string(), "40k calls a day".to_string()]);
        let again = e.on_external_cards(8, vec![raw(CardKind::Fact, "40k calls a day", "")], None, 6);
        assert!(again.added.is_empty(), "a dismissed card is not shown again");
    }

    #[test]
    fn research_cards_carry_their_origin_and_their_own_ids() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "What does nova-3 streaming cost per minute?", t0);
        let applied = e.on_external_cards(
            1,
            vec![raw(CardKind::Fact, "$0.0077/min pay-as-you-go", "What does nova-3 streaming cost per minute?")],
            Some("DeepHive".into()),
            9,
        );
        assert_eq!(applied.added.len(), 1);
        assert!(applied.added[0].id.starts_with("assist-r1-"), "{}", applied.added[0].id);
        assert_eq!(applied.added[0].origin.as_deref(), Some("DeepHive"));
        assert_eq!(e.cards()[0].origin.as_deref(), Some("DeepHive"), "the engine's copy carries it too");
        let fast = e.on_external_cards(1, vec![raw(CardKind::Ask, "Which volume?", "cost per minute")], None, 10);
        assert!(fast.added[0].id.starts_with("assist-p1-"), "a fast call with the same id cannot collide");
        assert_eq!(fast.added[0].origin, None);
    }

    #[test]
    fn sidecar_cards_quoting_a_retracted_line_fall_back_to_the_last_speaker_out() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "Send me the SOC 2 report.", t0);
        e.on_line(2, Side::Me, "Send me the SOC 2 report.", t0); // bleed
        e.on_retract(2);
        let applied = e.on_external_cards(1, vec![raw(CardKind::Todo, "Send SOC 2", "Send me the SOC 2 report")], None, 0);
        assert_eq!(applied.added[0].source_seq, 1, "never tied to the retracted Mic-In copy");
    }

    // ---- trigger and debounce -------------------------------------------

    #[test]
    fn a_speaker_out_final_triggers_after_the_settle_time() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "What does pricing look like?", t0);
        assert!(e.poll(t0 + ms(1499), true).is_none(), "fired before the turn settled");
        let call = e.poll(t0 + ms(1500), true).expect("due after 1.5 s");
        assert_eq!(call.lines.len(), 1);
    }

    #[test]
    fn mic_in_lines_never_trigger() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Me, "So what do you need?", t0);
        e.on_line(2, Side::Unknown, "unlabelled", t0);
        assert!(e.poll(t0 + Duration::from_secs(10), true).is_none());
    }

    #[test]
    fn mic_in_lines_are_context_for_the_next_call() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Me, "Hi", t0);
        e.on_line(2, Side::Them, "Hello", t0 + ms(100));
        let call = e.poll(t0 + ms(2000), true).unwrap();
        let seqs: Vec<u64> = call.lines.iter().map(|l| l.seq).collect();
        assert_eq!(seqs, vec![1, 2]);
    }

    #[test]
    fn each_new_speaker_out_final_restarts_the_settle_timer() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "First half,", t0);
        e.on_line(2, Side::Them, "second half.", t0 + ms(1000));
        assert!(e.poll(t0 + ms(1600), true).is_none(), "the turn was still going");
        let call = e.poll(t0 + ms(2500), true).expect("settled 1.5 s after the last final");
        assert_eq!(call.lines.len(), 2, "one call covers the whole turn");
    }

    #[test]
    fn only_one_call_in_flight() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "a", t0);
        let first = e.poll(t0 + ms(1500), true).unwrap();
        e.on_line(2, Side::Them, "b", t0 + ms(2000));
        assert!(e.poll(t0 + Duration::from_secs(30), true).is_none(), "second call while the first is in flight");
        e.on_call_done(first.call_id, vec![], 0);
        assert!(e.poll(t0 + Duration::from_secs(30), true).is_some(), "the pending trigger fires after the result");
    }

    #[test]
    fn at_most_one_call_every_five_seconds() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "a", t0);
        let first = e.poll(t0 + ms(1500), true).unwrap();
        e.on_call_done(first.call_id, vec![], 0);
        e.on_line(2, Side::Them, "b", t0 + ms(1600));
        assert!(e.poll(t0 + ms(3100), true).is_none(), "settled, but only 1.6 s after the last call");
        assert!(e.poll(t0 + ms(6499), true).is_none());
        assert!(e.poll(t0 + ms(6500), true).is_some(), "5 s after the previous call started");
    }

    #[test]
    fn a_failed_call_frees_the_slot_but_keeps_the_rate_limit() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "a", t0);
        let first = e.poll(t0 + ms(1500), true).unwrap();
        e.on_call_failed(first.call_id);
        assert_eq!(e.in_flight(), None);
        e.on_line(2, Side::Them, "b", t0 + ms(1600));
        assert!(e.poll(t0 + ms(3200), true).is_none());
        assert!(e.poll(t0 + ms(6500), true).is_some());
    }

    #[test]
    fn disabled_never_calls_and_drops_the_pending_trigger() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "a", t0);
        assert!(e.poll(t0 + ms(2000), false).is_none());
        assert!(e.poll(t0 + ms(2100), true).is_none(), "a trigger from while it was off fired later");
        e.on_line(2, Side::Them, "b", t0 + ms(3000));
        assert!(e.poll(t0 + ms(4500), true).is_some(), "new speech after turning on still triggers");
    }

    #[test]
    fn a_late_result_does_not_clear_the_newer_call_in_flight() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "a", t0);
        let first = e.poll(t0 + ms(1500), true).unwrap();
        e.on_call_failed(first.call_id); // cancelled (assist turned off and on again)
        e.on_line(2, Side::Them, "b", t0 + ms(5000));
        let second = e.poll(t0 + ms(6500), true).unwrap();
        e.on_call_done(first.call_id, vec![raw(CardKind::Ask, "late", "a")], 0);
        assert_eq!(e.in_flight(), Some(second.call_id), "the late result cleared the newer call");
        let applied = e.on_call_done(second.call_id, vec![raw(CardKind::Ask, "fresh", "b")], 0);
        assert_eq!(applied.added.len(), 1);
    }

    #[test]
    fn a_stale_result_is_ignored() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "a", t0);
        let call = e.poll(t0 + ms(1500), true).unwrap();
        e.on_call_failed(call.call_id);
        let applied = e.on_call_done(call.call_id, vec![raw(CardKind::Ask, "late", "a")], 0);
        assert!(applied.added.is_empty());
        assert!(e.cards().is_empty());
    }

    // ---- context window -------------------------------------------------

    #[test]
    fn the_window_keeps_the_last_12_lines() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        for i in 0..20 {
            e.on_line(i, Side::Me, "x", t0);
        }
        e.on_line(20, Side::Them, "q", t0);
        let call = e.poll(t0 + ms(1500), true).unwrap();
        assert_eq!(call.lines.len(), 12);
        assert_eq!(call.lines.first().unwrap().seq, 9);
    }

    #[test]
    fn the_window_drops_lines_older_than_two_minutes() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Me, "old", t0);
        e.on_line(2, Side::Them, "new", t0 + Duration::from_secs(119));
        let call = e.poll(t0 + Duration::from_secs(121), true).unwrap();
        assert_eq!(call.lines.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![2]);
    }

    // ---- retraction -----------------------------------------------------

    #[test]
    fn a_retracted_line_leaves_the_window() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Me, "bleed copy", t0);
        e.on_line(2, Side::Them, "real line", t0);
        e.on_retract(1);
        let call = e.poll(t0 + ms(1500), true).unwrap();
        assert_eq!(call.lines.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn retracting_the_only_speaker_out_line_cancels_the_trigger() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Me, "hi", t0);
        e.on_line(2, Side::Them, "ghost", t0);
        e.on_retract(2);
        assert!(e.poll(t0 + ms(2000), true).is_none());
    }

    #[test]
    fn a_retraction_withdraws_cards_built_from_that_line_even_pinned() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Me, "we can do forty thousand calls", t0);
        e.on_line(2, Side::Them, "what about pricing", t0);
        let call = e.poll(t0 + ms(1500), true).unwrap();
        let applied = e.on_call_done(
            call.call_id,
            vec![
                raw(CardKind::Fact, "Capacity: 40k calls", "forty thousand calls"),
                raw(CardKind::Ask, "Volume tier?", "what about pricing"),
            ],
            0,
        );
        let bleed_card = applied.added.iter().find(|c| c.source_seq == 1).unwrap().id.clone();
        e.set_pinned(&bleed_card, true);
        assert_eq!(e.on_retract(1), vec![bleed_card]);
        assert_eq!(e.cards().len(), 1);
        assert_eq!(e.cards()[0].source_seq, 2);
        assert!(e.pinned().is_empty());
    }

    #[test]
    fn a_line_retracted_while_the_call_is_in_flight_yields_no_card() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Me, "account numbers are mangled", t0);
        e.on_line(2, Side::Them, "account numbers are mangled by our vendor", t0);
        let call = e.poll(t0 + ms(1500), true).unwrap();
        e.on_retract(1);
        let applied = e.on_call_done(
            call.call_id,
            vec![
                raw(CardKind::Ask, "Which digits?", "account numbers are mangled"),
                raw(CardKind::Todo, "Ask for samples", "nothing like any line at all"),
            ],
            0,
        );
        // "account numbers are mangled" is contained in line 2 too, and the
        // newest containing line wins, so the first card survives on line 2.
        assert_eq!(applied.added.iter().map(|c| c.source_seq).collect::<Vec<_>>(), vec![2, 2]);

        // A quote only line 1 holds is dropped.
        e.on_line(3, Side::Me, "we bill per minute", t0 + ms(7000));
        e.on_line(4, Side::Them, "ok", t0 + ms(7000));
        let call = e.poll(t0 + ms(9000), true).unwrap();
        e.on_retract(3);
        let applied = e.on_call_done(call.call_id, vec![raw(CardKind::Fact, "Per-minute billing", "we bill per minute")], 0);
        assert!(applied.added.is_empty());
    }

    // ---- cards ----------------------------------------------------------

    #[test]
    fn quotes_map_to_lines_and_unmatched_fall_back_to_the_last_speaker_out() {
        let t0 = Instant::now();
        let lines = vec![
            Line { seq: 1, side: Side::Them, text: "We process forty thousand calls a day.".into(), at: t0 },
            Line { seq: 2, side: Side::Me, text: "Got it, thanks.".into(), at: t0 },
            Line { seq: 3, side: Side::Them, text: "What about pricing at that volume?".into(), at: t0 },
            Line { seq: 4, side: Side::Me, text: "Good question.".into(), at: t0 },
        ];
        assert_eq!(source_line(&lines, "forty thousand calls"), Some(1));
        assert_eq!(source_line(&lines, "What about Pricing, at that volume"), Some(3));
        assert_eq!(source_line(&lines, "pricing at volume what"), Some(3), "word overlap");
        assert_eq!(source_line(&lines, "nothing like it"), Some(3), "fallback: newest Speaker-Out");
        assert_eq!(source_line(&lines, ""), Some(3));
        assert_eq!(source_line(&[], "x"), None);
    }

    #[test]
    fn a_call_carries_the_cards_already_shown_newest_first() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "a", t0);
        let c1 = e.poll(t0 + ms(1500), true).unwrap();
        assert!(c1.shown.is_empty());
        e.on_call_done(c1.call_id, vec![raw(CardKind::Ask, "First", "a"), raw(CardKind::Todo, "Second", "a")], 0);
        e.on_line(2, Side::Them, "b", t0 + ms(6000));
        let c2 = e.poll(t0 + ms(7500), true).unwrap();
        assert_eq!(c2.shown, vec!["First".to_string(), "Second".to_string()]);
    }

    #[test]
    fn a_dismissed_card_is_not_suggested_again() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "a", t0);
        let c1 = e.poll(t0 + ms(1500), true).unwrap();
        let id = e.on_call_done(c1.call_id, vec![raw(CardKind::Ask, "Budget?", "a")], 0).added[0].id.clone();
        assert!(e.dismiss(&id));
        e.on_line(2, Side::Them, "b", t0 + ms(6000));
        let c2 = e.poll(t0 + ms(7500), true).unwrap();
        assert_eq!(c2.shown, vec!["Budget?".to_string()], "the model is told about it");
        let applied = e.on_call_done(c2.call_id, vec![raw(CardKind::Ask, "budget", "b")], 0);
        assert!(applied.added.is_empty(), "and a repeat is dropped anyway");
    }

    #[test]
    fn newest_card_on_top_and_repeats_are_dropped() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config::default());
        e.on_line(1, Side::Them, "a", t0);
        let c1 = e.poll(t0 + ms(1500), true).unwrap();
        e.on_call_done(c1.call_id, vec![raw(CardKind::Ask, "Old one", "a")], 1);
        e.on_line(2, Side::Them, "b", t0 + ms(6000));
        let c2 = e.poll(t0 + ms(7500), true).unwrap();
        let applied = e.on_call_done(
            c2.call_id,
            vec![raw(CardKind::Ask, "old one.", "b"), raw(CardKind::Todo, "New one", "b")],
            2,
        );
        assert_eq!(applied.added.len(), 1, "a repeat of a shown card is dropped");
        let texts: Vec<&str> = e.cards().iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["New one", "Old one"]);
    }

    #[test]
    fn dismiss_pin_and_eviction() {
        let t0 = Instant::now();
        let mut e = Engine::new(Config { max_unpinned_cards: 2, ..Config::default() });
        let mut now = t0;
        let mut ids = Vec::new();
        for i in 0..3u64 {
            e.on_line(i, Side::Them, &format!("line {i}"), now);
            let call = e.poll(now + ms(1500), true).unwrap();
            let applied = e.on_call_done(call.call_id, vec![raw(CardKind::Ask, &format!("card {i}"), "")], 0);
            ids.push(applied.added[0].id.clone());
            if i == 0 {
                assert!(e.set_pinned(&ids[0], true).unwrap().pinned);
            }
            if i == 2 {
                assert!(applied.evicted.is_empty(), "the pinned card does not count");
            }
            now += Duration::from_secs(6);
        }
        assert_eq!(e.cards().len(), 3);
        e.on_line(9, Side::Them, "line 9", now);
        let call = e.poll(now + ms(1500), true).unwrap();
        let applied = e.on_call_done(call.call_id, vec![raw(CardKind::Ask, "card 9", "")], 0);
        assert_eq!(applied.evicted, vec![ids[1].clone()], "the oldest unpinned card goes");
        assert_eq!(e.pinned().len(), 1);
        assert!(e.dismiss(&ids[2]));
        assert!(!e.dismiss(&ids[2]));
        assert!(e.set_pinned("nope", true).is_none());
    }
}
