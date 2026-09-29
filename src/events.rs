use chrono::{DateTime, Local, NaiveDateTime, SecondsFormat, TimeDelta, TimeZone, Utc};
use serde_json::Value;

use crate::api::{self, Session};
use crate::config::Config;
use crate::{CYAN, DIM, Error, Opts, RESET, YELLOW, c};

const SELECT: &str = "subject,start,end,isAllDay,location,isCancelled,showAs";
const PAGE: u32 = 50;

struct Event {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    subject: String,
    location: Option<String>,
    all_day: bool,
    cancelled: bool,
}

pub async fn run(config: &Config, window: TimeDelta, opts: &Opts) -> Result<(), Error> {
    let session = Session::new(&api::client(), config.token.clone());
    // From now rather than from midnight: "the next N days" is forward-looking,
    // and a meeting that finished this morning isn't what's being asked about.
    let from = Utc::now();
    let url = calendar_url(from, from + window);

    let values = session
        .get_all(&url, opts.verbose)
        .await
        .map_err(|e| crate::config::explain(e, &config.token, "Calendars.Read"))?;

    let mut events: Vec<Event> = values.iter().filter_map(event_from).collect();
    // calendarView is asked for them in order, but a series expanded into
    // occurrences has come back out of order before, and a calendar that isn't
    // chronological is no calendar.
    events.sort_by_key(|e| e.start);

    if events.is_empty() {
        eprintln!("nothing in the next {}", describe(window));
        return Ok(());
    }
    for event in &events {
        print_event(event);
    }
    Ok(())
}

// calendarView rather than /me/events: /me/events hands back a recurring
// meeting once, as the series it was defined as, while this expands it into the
// occurrences that actually fall in the window — which is what a calendar is.
fn calendar_url(from: DateTime<Utc>, to: DateTime<Utc>) -> String {
    format!(
        "https://graph.microsoft.com/v1.0/me/calendarView?startDateTime={}&endDateTime={}\
&$select={SELECT}&$orderby=start/dateTime&$top={PAGE}",
        api::encode(&stamp(from)),
        api::encode(&stamp(to))
    )
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn event_from(v: &Value) -> Option<Event> {
    let start = moment(v.get("start"))?;
    let end = moment(v.get("end")).unwrap_or(start);
    Some(Event {
        start,
        end,
        subject: v
            .get("subject")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("(no subject)")
            .to_string(),
        location: v
            .get("location")
            .and_then(|l| l.get("displayName"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string),
        all_day: v.get("isAllDay").and_then(Value::as_bool).unwrap_or(false),
        cancelled: v.get("isCancelled").and_then(Value::as_bool).unwrap_or(false),
    })
}

// Outlook's dateTime is not RFC 3339: it carries seven fractional digits and no
// zone at all — "2026-09-29T09:00:00.0000000" — with the zone in a field beside
// it. Asking for UTC (see api::client) is what makes that field predictable.
fn moment(at: Option<&Value>) -> Option<DateTime<Utc>> {
    let at = at?;
    let text = at.get("dateTime").and_then(Value::as_str)?;
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    match at.get("timeZone").and_then(Value::as_str) {
        // Anything else would need a zone database to resolve, and asking for
        // UTC means it shouldn't arise; reading it as local is the closest
        // guess and better than dropping the event.
        Some("UTC") | None => Some(Utc.from_utc_datetime(&naive)),
        Some(_) => Local.from_local_datetime(&naive).single().map(|at| at.with_timezone(&Utc)),
    }
}

fn print_event(event: &Event) {
    let start = event.start.with_timezone(&Local);
    // An all-day event ends at midnight of the following day, so the day it
    // would print is one too many.
    let day = start.format("%d.%m.%y");
    let span = if event.all_day {
        "all day".to_string()
    } else {
        format!("{}-{}", start.format("%H:%M"), event.end.with_timezone(&Local).format("%H:%M"))
    };
    let mark = if event.cancelled { "!" } else { " " };
    let subject = if event.cancelled { format!("{} (cancelled)", event.subject) } else { event.subject.clone() };
    let where_ = match &event.location {
        Some(place) => format!("  {}({place}){}", c(DIM), c(RESET)),
        None => String::new(),
    };
    outln!(
        "{}[{day} {span:<11}]{} {}{mark}{} {}{subject}{}{where_}",
        c(DIM),
        c(RESET),
        c(YELLOW),
        c(RESET),
        c(CYAN),
        c(RESET)
    );
}

fn describe(window: TimeDelta) -> String {
    let secs = window.num_seconds().max(0) as u64;
    match secs {
        s if s % 86_400 == 0 && s >= 86_400 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 && s >= 3_600 => format!("{}h", s / 3_600),
        s => format!("{}m", s / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn the_url_asks_for_the_window_as_occurrences() {
        let url = calendar_url(at("2026-09-29T08:00:00Z"), at("2026-10-06T08:00:00Z"));
        assert!(url.starts_with("https://graph.microsoft.com/v1.0/me/calendarView?"), "got: {url}");
        assert!(url.contains("startDateTime=2026-09-29T08%3A00%3A00Z"), "got: {url}");
        assert!(url.contains("endDateTime=2026-10-06T08%3A00%3A00Z"), "got: {url}");
        assert!(url.contains("$orderby=start/dateTime"), "got: {url}");
    }

    // The format that tripped this up: seven fractional digits, no zone.
    #[test]
    fn outlooks_own_date_format_is_read() {
        let start = moment(Some(&json!({"dateTime": "2026-09-29T09:00:00.0000000", "timeZone": "UTC"})));
        assert_eq!(start, Some(at("2026-09-29T09:00:00Z")));

        // And the same thing without the fraction, in case it ever drops it.
        let plain = moment(Some(&json!({"dateTime": "2026-09-29T09:00:00", "timeZone": "UTC"})));
        assert_eq!(plain, Some(at("2026-09-29T09:00:00Z")));
    }

    #[test]
    fn something_that_is_not_a_moment_is_not_guessed_at() {
        assert_eq!(moment(None), None);
        assert_eq!(moment(Some(&json!({"timeZone": "UTC"}))), None);
        assert_eq!(moment(Some(&json!({"dateTime": "whenever", "timeZone": "UTC"}))), None);
    }

    #[test]
    fn an_event_is_read_from_the_fields_graph_returns() {
        let e = event_from(&json!({
            "subject": "Daily Standup",
            "start": {"dateTime": "2026-09-29T07:00:00.0000000", "timeZone": "UTC"},
            "end": {"dateTime": "2026-09-29T07:30:00.0000000", "timeZone": "UTC"},
            "isAllDay": false, "isCancelled": false,
            "location": {"displayName": "Microsoft Teams"}
        }))
        .expect("has a start");

        assert_eq!(e.subject, "Daily Standup");
        assert_eq!(e.location.as_deref(), Some("Microsoft Teams"));
        assert_eq!(e.start, at("2026-09-29T07:00:00Z"));
        assert!(!e.all_day);
    }

    // A meeting with no room set has no location, which is not a blank one.
    #[test]
    fn an_empty_location_reads_as_absent() {
        let e = event_from(&json!({
            "start": {"dateTime": "2026-09-29T07:00:00.0000000", "timeZone": "UTC"},
            "location": {"displayName": "  "}
        }))
        .unwrap();
        assert_eq!(e.location, None);
    }

    // Without a start there is nowhere to put it in the list.
    #[test]
    fn an_event_without_a_start_is_skipped() {
        assert!(event_from(&json!({"subject": "whenever"})).is_none());
    }

    // An event with no end is a point in time, not a broken record.
    #[test]
    fn a_missing_end_falls_back_to_the_start() {
        let e = event_from(&json!({
            "start": {"dateTime": "2026-09-29T07:00:00.0000000", "timeZone": "UTC"}
        }))
        .unwrap();
        assert_eq!(e.end, e.start);
    }
}
