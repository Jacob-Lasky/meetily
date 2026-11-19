//! Calendar events as the Google Calendar MCP server returns them, and which
//! of them Meetily records and prepares a brief for (WORK-677). Pure: no I/O.
//!
//! The server's `list_events` answers with the Calendar API's event resource
//! under `events` (the REST API calls the list `items`; both are read). It
//! adds a `conferenceUrl` convenience field; `hangoutLink`,
//! `conferenceData.entryPoints` and a meeting URL in the location or
//! description are read too, so another calendar server shaped like the REST
//! API works the same way.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attendee {
    pub email: String,
    pub name: Option<String>,
    pub is_self: bool,
    pub organizer: bool,
    /// accepted, declined, tentative, needsAction.
    pub response: String,
    pub optional: bool,
    /// A room or other resource, not a person.
    pub resource: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalEvent {
    /// Unique per instance (a recurring event's instances carry a suffix).
    pub id: String,
    pub title: String,
    pub description: String,
    pub location: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    pub attendees: Vec<Attendee>,
    pub organizer_email: String,
    pub organizer_is_self: bool,
    pub video_url: Option<String>,
    /// Shown as free (`transparency: transparent`).
    pub free: bool,
    /// Lower case, letters only: `default`, `focustime`, `outofoffice`,
    /// `workinglocation`, `birthday`, `fromgmail`.
    pub event_type: String,
    pub cancelled: bool,
}

impl CalEvent {
    /// Me's own answer: the attendee marked `self`, else `accepted` for an
    /// event Me organizes (an event of one's own has no attendee list).
    pub fn self_response(&self) -> Option<&str> {
        if let Some(me) = self.attendees.iter().find(|a| a.is_self) {
            return Some(me.response.as_str());
        }
        self.organizer_is_self.then_some("accepted")
    }

    /// People other than Me (rooms and resources do not count).
    pub fn others(&self) -> impl Iterator<Item = &Attendee> {
        self.attendees.iter().filter(|a| !a.is_self && !a.resource)
    }

    /// Me's email domain, from the self attendee or Me as organizer.
    pub fn own_domain(&self) -> Option<String> {
        let email = self
            .attendees
            .iter()
            .find(|a| a.is_self)
            .map(|a| a.email.as_str())
            .or_else(|| self.organizer_is_self.then_some(self.organizer_email.as_str()))?;
        domain_of(email)
    }

    /// Attendees from another organization than Me's.
    pub fn external(&self) -> Vec<&Attendee> {
        let own = self.own_domain();
        self.others().filter(|a| domain_of(&a.email) != own).collect()
    }
}

pub fn domain_of(email: &str) -> Option<String> {
    email.rsplit_once('@').map(|(_, d)| d.trim().to_lowercase()).filter(|d| !d.is_empty())
}

/// Mail providers whose domain names no organization.
const PERSONAL_DOMAINS: [&str; 9] = [
    "gmail.com",
    "googlemail.com",
    "outlook.com",
    "hotmail.com",
    "live.com",
    "yahoo.com",
    "icloud.com",
    "me.com",
    "proton.me",
];

/// "acme" for acme.com / mail.acme.co.uk; None for a personal mail domain.
pub fn organization_of(domain: &str) -> Option<String> {
    if PERSONAL_DOMAINS.contains(&domain) {
        return None;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    // The label before the public suffix: skip a two-letter country code
    // and a second-level label such as co/com/org in front of it.
    let mut i = labels.len().checked_sub(2)?;
    if labels.len() >= 3 && labels[labels.len() - 1].len() == 2 && ["co", "com", "org", "net", "ac", "gov"].contains(&labels[i]) {
        i -= 1;
    }
    Some(labels[i].to_string())
}

/// Read a `list_events` tool result's text. `next` is the page token, if any.
pub fn parse_events(text: &str) -> Result<(Vec<CalEvent>, Option<String>), String> {
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("the calendar's reply is not JSON: {e}"))?;
    let list = value
        .get("events")
        .or_else(|| value.get("items"))
        .and_then(|v| v.as_array())
        .ok_or("the calendar's reply holds no events list")?;
    let next = value.get("nextPageToken").and_then(|v| v.as_str()).map(str::to_string);
    Ok((list.iter().filter_map(parse_event).collect(), next))
}

fn s<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

fn b(v: &serde_json::Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

/// A start or end: `dateTime` (timed) or `date` (all day, read as midnight UTC).
fn when(v: Option<&serde_json::Value>) -> Option<(DateTime<Utc>, bool)> {
    let v = v?;
    if let Some(dt) = v.get("dateTime").and_then(|x| x.as_str()) {
        return DateTime::parse_from_rfc3339(dt).ok().map(|t| (t.with_timezone(&Utc), false));
    }
    let date = NaiveDate::parse_from_str(v.get("date")?.as_str()?, "%Y-%m-%d").ok()?;
    Some((date.and_hms_opt(0, 0, 0)?.and_utc(), true))
}

fn parse_event(v: &serde_json::Value) -> Option<CalEvent> {
    let id = s(v, "id").to_string();
    if id.is_empty() {
        return None;
    }
    let (start, all_day) = when(v.get("start"))?;
    let (end, _) = when(v.get("end"))?;
    let attendees = v
        .get("attendees")
        .and_then(|a| a.as_array())
        .map(|list| {
            list.iter()
                .map(|a| Attendee {
                    email: s(a, "email").to_string(),
                    name: Some(s(a, "displayName").to_string()).filter(|n| !n.is_empty()),
                    is_self: b(a, "self"),
                    organizer: b(a, "organizer"),
                    response: s(a, "responseStatus").to_string(),
                    optional: b(a, "optionalAttendee"),
                    resource: b(a, "resource"),
                })
                .collect()
        })
        .unwrap_or_default();
    let organizer = v.get("organizer");
    let description = s(v, "description").to_string();
    let location = s(v, "location").to_string();
    Some(CalEvent {
        title: s(v, "summary").trim().to_string(),
        video_url: video_url(v, &location, &description),
        description,
        location,
        start,
        end,
        all_day,
        attendees,
        organizer_email: organizer.map(|o| s(o, "email").to_string()).unwrap_or_default(),
        organizer_is_self: organizer.map(|o| b(o, "self")).unwrap_or(false),
        free: s(v, "transparency") == "transparent",
        event_type: s(v, "eventType").chars().filter(|c| c.is_ascii_alphabetic()).collect::<String>().to_lowercase(),
        cancelled: s(v, "status") == "cancelled",
        id,
    })
}

/// Hosts whose links are video calls.
const VIDEO_HOSTS: [&str; 7] = [
    "meet.google.com/",
    "zoom.us/",
    "teams.microsoft.com/",
    "teams.live.com/",
    "webex.com/",
    "whereby.com/",
    "chime.aws/",
];

fn video_url(v: &serde_json::Value, location: &str, description: &str) -> Option<String> {
    for key in ["conferenceUrl", "hangoutLink"] {
        let url = s(v, key);
        if !url.is_empty() {
            return Some(url.to_string());
        }
    }
    if let Some(points) = v.pointer("/conferenceData/entryPoints").and_then(|p| p.as_array()) {
        if let Some(p) = points.iter().find(|p| s(p, "entryPointType") == "video" && !s(p, "uri").is_empty()) {
            return Some(s(p, "uri").to_string());
        }
    }
    [location, description].into_iter().find_map(|text| {
        text.split(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '(' | ')'))
            .find(|w| w.starts_with("https://") && VIDEO_HOSTS.iter().any(|h| w.contains(h)))
            .map(str::to_string)
    })
}

/// Which meetings are recorded and prepared. Stored in Settings; see
/// `Default` for what a fresh install records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Rules {
    /// Me accepted (or organizes) it.
    pub require_accepted: bool,
    /// Someone besides Me is invited.
    pub require_others: bool,
    /// It has a video-call link.
    pub require_video: bool,
    /// More people than this (Me excluded) is a broadcast, not a meeting;
    /// 0 means no limit.
    pub max_attendees: u32,
    /// A title containing one of these qualifies even with no other
    /// attendee on the invite (an interview booked by a scheduling tool
    /// invites only the interviewer).
    pub include_keywords: Vec<String>,
    /// A title containing one of these never qualifies.
    pub exclude_keywords: Vec<String>,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            require_accepted: true,
            require_others: true,
            require_video: true,
            max_attendees: 25,
            include_keywords: vec!["interview".into()],
            exclude_keywords: Vec::new(),
        }
    }
}

fn title_has(title: &str, keywords: &[String]) -> Option<String> {
    let lower = title.to_lowercase();
    keywords.iter().map(|k| k.trim()).find(|k| !k.is_empty() && lower.contains(&k.to_lowercase())).map(str::to_string)
}

/// Ok when `ev` qualifies under `rules`, else the reason it does not (shown
/// in Settings next to the event). All-day, cancelled, free, focus-time and
/// out-of-office events and ones Me declined never qualify.
pub fn qualifies(ev: &CalEvent, rules: &Rules) -> Result<(), String> {
    if ev.cancelled {
        return Err("cancelled".into());
    }
    if ev.all_day {
        return Err("all day".into());
    }
    match ev.event_type.as_str() {
        "focustime" => return Err("focus time".into()),
        "outofoffice" => return Err("out of office".into()),
        "workinglocation" | "birthday" => return Err("not a meeting".into()),
        _ => {}
    }
    if ev.free {
        return Err("shown as free".into());
    }
    if let Some(k) = title_has(&ev.title, &rules.exclude_keywords) {
        return Err(format!("title contains \"{k}\""));
    }
    let response = ev.self_response();
    if response == Some("declined") {
        return Err("you declined".into());
    }
    if rules.require_accepted && response != Some("accepted") {
        return Err("you have not accepted".into());
    }
    let others = ev.others().count();
    let included = title_has(&ev.title, &rules.include_keywords).is_some();
    if rules.require_others && others == 0 && !included {
        return Err("no one else is invited".into());
    }
    if rules.max_attendees > 0 && others > rules.max_attendees as usize {
        return Err(format!("{others} people invited (the limit is {})", rules.max_attendees));
    }
    if rules.require_video && ev.video_url.is_none() {
        return Err("no video-call link".into());
    }
    Ok(())
}

/// An interview: its prep never leaves the desktop for a knowledge service
/// (candidate data stays local, WORK-671), and only the calendar and earlier
/// Meetily meetings feed the brief.
pub fn is_interview(ev: &CalEvent) -> bool {
    let title = ev.title.to_lowercase();
    let description = ev.description.to_lowercase();
    title.contains("interview")
        || ["ashbyhq.com", "greenhouse.io", "lever.co", "workable.com"].iter().any(|d| description.contains(d))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> serde_json::Value {
        serde_json::json!({
            "summary": "Jake Lasky (Deepgram)",
            "nextPageToken": "abc",
            "events": [
                {
                    "id": "e1_20261119T170000Z",
                    "status": "confirmed",
                    "summary": "Deepgram | Acme - Technical deep dive ",
                    "description": "Agenda: latency",
                    "start": {"dateTime": "2026-11-19T12:00:00-05:00", "timeZone": "America/Los_Angeles"},
                    "end": {"dateTime": "2026-11-19T13:00:00-05:00"},
                    "eventType": "DEFAULT",
                    "conferenceUrl": "https://meet.google.com/abc-defg-hij",
                    "organizer": {"email": "jacob.lasky@deepgram.com", "self": true},
                    "attendees": [
                        {"email": "jacob.lasky@deepgram.com", "self": true, "organizer": true, "responseStatus": "accepted"},
                        {"email": "pat@acme.co.uk", "displayName": "Pat Lee", "responseStatus": "needsAction"},
                        {"email": "room-4@resource.calendar.google.com", "resource": true, "responseStatus": "accepted"}
                    ]
                },
                {"id": "e2", "summary": "Holiday", "start": {"date": "2026-11-20"}, "end": {"date": "2026-11-21"}},
                {"id": "e3", "summary": "Focus", "eventType": "FOCUS_TIME",
                 "start": {"dateTime": "2026-11-19T14:00:00Z"}, "end": {"dateTime": "2026-11-19T15:00:00Z"}},
                {"id": "", "summary": "no id", "start": {"dateTime": "2026-11-19T14:00:00Z"}, "end": {"dateTime": "2026-11-19T15:00:00Z"}}
            ]
        })
    }

    fn events() -> Vec<CalEvent> {
        parse_events(&sample().to_string()).unwrap().0
    }

    #[test]
    fn events_are_parsed_from_the_mcp_reply() {
        let (list, next) = parse_events(&sample().to_string()).unwrap();
        assert_eq!(next.as_deref(), Some("abc"));
        assert_eq!(list.len(), 3, "the event with no id is skipped");
        let e = &list[0];
        assert_eq!(e.title, "Deepgram | Acme - Technical deep dive");
        assert_eq!(e.start.to_rfc3339(), "2026-11-19T17:00:00+00:00");
        assert_eq!(e.video_url.as_deref(), Some("https://meet.google.com/abc-defg-hij"));
        assert_eq!(e.self_response(), Some("accepted"));
        assert_eq!(e.others().count(), 1, "the room is not a person");
        assert_eq!(e.external().len(), 1);
        assert_eq!(e.own_domain().as_deref(), Some("deepgram.com"));
        assert!(list[1].all_day);
        assert_eq!(list[2].event_type, "focustime");
        // The REST API's `items` list reads the same way.
        let rest = serde_json::json!({"items": sample()["events"]});
        assert_eq!(parse_events(&rest.to_string()).unwrap().0.len(), 3);
        assert!(parse_events("not json").is_err());
        assert!(parse_events("{}").is_err());
    }

    #[test]
    fn default_rules_take_a_real_meeting_and_skip_the_rest() {
        let list = events();
        let rules = Rules::default();
        assert_eq!(qualifies(&list[0], &rules), Ok(()));
        assert_eq!(qualifies(&list[1], &rules), Err("all day".into()));
        assert_eq!(qualifies(&list[2], &rules), Err("focus time".into()));
    }

    #[test]
    fn each_rule_can_reject_and_be_turned_off() {
        let base = events().remove(0);
        let rules = Rules::default();

        let mut declined = base.clone();
        declined.attendees[0].response = "declined".into();
        assert_eq!(qualifies(&declined, &rules), Err("you declined".into()));
        // Declined is final even with the accepted rule off.
        assert!(qualifies(&declined, &Rules { require_accepted: false, ..rules.clone() }).is_err());

        let mut pending = base.clone();
        pending.attendees[0].response = "needsAction".into();
        assert_eq!(qualifies(&pending, &rules), Err("you have not accepted".into()));
        assert_eq!(qualifies(&pending, &Rules { require_accepted: false, ..rules.clone() }), Ok(()));

        let mut alone = base.clone();
        alone.attendees.retain(|a| a.is_self || a.resource);
        assert_eq!(qualifies(&alone, &rules), Err("no one else is invited".into()));
        assert_eq!(qualifies(&alone, &Rules { require_others: false, ..rules.clone() }), Ok(()));
        // An interview booked by a scheduler invites only the interviewer.
        let mut interview = alone.clone();
        interview.title = "Interview (Applied Engineer) - Sam".into();
        assert_eq!(qualifies(&interview, &rules), Ok(()));

        let mut no_video = base.clone();
        no_video.video_url = None;
        assert_eq!(qualifies(&no_video, &rules), Err("no video-call link".into()));
        assert_eq!(qualifies(&no_video, &Rules { require_video: false, ..rules.clone() }), Ok(()));

        let mut big = base.clone();
        for i in 0..30 {
            big.attendees.push(Attendee {
                email: format!("p{i}@deepgram.com"),
                name: None,
                is_self: false,
                organizer: false,
                response: "accepted".into(),
                optional: false,
                resource: false,
            });
        }
        assert_eq!(qualifies(&big, &rules), Err("31 people invited (the limit is 25)".into()));
        assert_eq!(qualifies(&big, &Rules { max_attendees: 0, ..rules.clone() }), Ok(()));

        let excluded = Rules { exclude_keywords: vec![" deep DIVE ".into()], ..rules.clone() };
        assert_eq!(qualifies(&base, &excluded), Err("title contains \"deep DIVE\"".into()));

        let mut free = base.clone();
        free.free = true;
        assert_eq!(qualifies(&free, &rules), Err("shown as free".into()));
        let mut cancelled = base;
        cancelled.cancelled = true;
        assert_eq!(qualifies(&cancelled, &rules), Err("cancelled".into()));
    }

    #[test]
    fn video_links_are_found_in_every_place_a_calendar_puts_them() {
        let ev = |extra: serde_json::Value| {
            let mut v = serde_json::json!({"id": "x", "start": {"dateTime": "2026-11-19T14:00:00Z"},
                                           "end": {"dateTime": "2026-11-19T15:00:00Z"}});
            v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            parse_event(&v).unwrap().video_url
        };
        assert_eq!(ev(serde_json::json!({"hangoutLink": "https://meet.google.com/x"})).as_deref(), Some("https://meet.google.com/x"));
        assert_eq!(
            ev(serde_json::json!({"conferenceData": {"entryPoints": [
                {"entryPointType": "phone", "uri": "tel:+1"},
                {"entryPointType": "video", "uri": "https://acme.zoom.us/j/1"}]}}))
            .as_deref(),
            Some("https://acme.zoom.us/j/1")
        );
        assert_eq!(
            ev(serde_json::json!({"description": "Join: <a href=\"https://teams.microsoft.com/l/meetup-join/1\">here</a>"})).as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/1")
        );
        assert_eq!(ev(serde_json::json!({"location": "Room 4, https://example.com/agenda"})), None);
    }

    #[test]
    fn organizations_come_from_work_domains_only() {
        assert_eq!(organization_of("acme.com").as_deref(), Some("acme"));
        assert_eq!(organization_of("mail.acme.co.uk").as_deref(), Some("acme"));
        assert_eq!(organization_of("gmail.com"), None);
        assert_eq!(organization_of("localhost"), None);
    }

    #[test]
    fn interviews_are_recognised_by_title_or_scheduler_link() {
        let mut e = events().remove(0);
        assert!(!is_interview(&e));
        e.description = "Details: https://app.ashbyhq.com/interview/123".into();
        assert!(is_interview(&e));
        e.description.clear();
        e.title = "Final INTERVIEW: Sam".into();
        assert!(is_interview(&e));
    }
}
