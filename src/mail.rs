use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Local, SecondsFormat, TimeDelta, Utc};
use serde_json::Value;

use crate::api::{self, Session};
use crate::config::Config;
use crate::{CYAN, DIM, Error, Opts, RESET, YELLOW, c};

// Only what a line needs. Graph returns the whole message otherwise, body and
// all, which for a week of mail is a lot of response to read and throw away.
const SELECT: &str = "id,receivedDateTime,subject,from,isRead";
const PAGE: u32 = 50;
// Enough that a message can't be printed twice across a few polls, bounded so a
// long-running follow doesn't grow without end.
const REMEMBER: usize = 2000;

struct Message {
    id: String,
    received: DateTime<Utc>,
    subject: String,
    from: String,
    unread: bool,
}

pub async fn run(
    config: &Config,
    window: TimeDelta,
    before: Option<DateTime<Utc>>,
    opts: &Opts,
) -> Result<(), Error> {
    let session = Session::new(&api::client(), config.token.clone());
    let until = before.unwrap_or_else(Utc::now);
    let mut since = until - window;
    let mut seen: HashSet<String> = HashSet::new();
    // Only needed to tell the two boxes apart, so only asked for when there is
    // an archive to put them in.
    let sent_folder = match &opts.mirror {
        Some(_) => crate::message::sent_folder_id(&session, opts).await,
        None => None,
    };
    // Sent mail is archived alongside what arrived, but not printed: this is a
    // view of what is coming in, and one's own replies interleaved with it is
    // not what either is for. It gets a cursor of its own because the two
    // folders fill at different rates.
    let mut sent_since = since;
    let mut sent_seen: HashSet<String> = HashSet::new();

    if opts.verbose {
        eprintln!("loading {} from {} to {}", config.folder, stamp(since), stamp(until));
    }
    let backfilled = print_new(&session, config, &mut since, before, &mut seen, sent_folder.as_deref(), opts)
        .await
        .map_err(|e| crate::config::explain(e, &config.token, "Mail.Read"))?;
    let sent = mirror_sent(&session, &mut sent_since, before, &mut sent_seen, sent_folder.as_deref(), opts).await?;
    if sent > 0 {
        eprintln!("{sent} sent message(s) mirrored");
    }
    if opts.verbose || backfilled == 0 {
        eprintln!("{backfilled} message(s) in the {} before {}", describe(window), stamp(until));
    }

    // A named window has an end, so there is nothing to wait for: following
    // would sit there watching a stretch of the past that cannot change. Same
    // reason myslackcli exits after a --before run, and what makes chunks
    // chainable in a script.
    if opts.once || before.is_some() {
        return Ok(());
    }

    // Graph has no way to push mail to a program without a public HTTPS
    // endpoint of its own, so this asks. The interval is configuration rather
    // than a constant because the right answer depends on the mailbox: see the
    // README.
    // Said every time, not only under --verbose: the backfill ends, the polling
    // makes no noise, and silence after the last line is indistinguishable from
    // a program that has hung. It goes to stderr, so a pipe into fzf is
    // unaffected.
    eprintln!(
        "following {} every {} — ctrl-c to stop, or --once to load and exit",
        config.folder,
        describe_std(config.poll)
    );
    loop {
        let interrupted = tokio::select! {
            _ = tokio::signal::ctrl_c() => true,
            _ = tokio::time::sleep(config.poll) => false,
        };
        if interrupted {
            break;
        }
        // One failed poll shouldn't end a session that's been running for
        // hours — but an expired token will fail every time from now on, so
        // that one stops.
        match print_new(&session, config, &mut since, None, &mut seen, sent_folder.as_deref(), opts).await {
            Ok(_) => {
                if let Err(e) =
                    mirror_sent(&session, &mut sent_since, None, &mut sent_seen, sent_folder.as_deref(), opts).await
                {
                    eprintln!("could not mirror sent mail ({e})");
                }
            }
            Err(e) if e.to_string().contains("expired") => return Err(e),
            Err(e) => eprintln!("poll failed ({e}) — trying again in {}", describe_std(config.poll)),
        }
    }
    Ok(())
}

// Fetches everything at or after `since`, prints what hasn't been printed
// before, and moves `since` up to the newest it saw.
async fn print_new(
    session: &Session,
    config: &Config,
    since: &mut DateTime<Utc>,
    until: Option<DateTime<Utc>>,
    seen: &mut HashSet<String>,
    sent_folder: Option<&str>,
    opts: &Opts,
) -> Result<usize, Error> {
    let url = messages_url(&config.folder, *since, until);
    let values = session.get_all(&url, opts.verbose).await?;

    let mut printed = 0;
    for value in &values {
        let Some(message) = message_from(value) else { continue };
        // `ge` rather than `gt` on the timestamp, because two messages can share
        // a second and `gt` would drop the second one. The id is what actually
        // decides whether this has been seen.
        if !seen.insert(message.id.clone()) {
            continue;
        }
        if message.received > *since {
            *since = message.received;
        }
        print_message(&message, opts.ids);
        printed += 1;

        // The listing carries no body, so each mirrored message is one more
        // request — which is why this only happens when a mirror was asked for.
        if let Some(archive) = &opts.mirror {
            match crate::message::render(session, &message.id, sent_folder, opts).await {
                Ok(rendered) => {
                    let file = archive.write(
                        rendered.which,
                        rendered.received,
                        &message.id,
                        &rendered.subject,
                        &rendered.document,
                    )?;
                    if opts.verbose {
                        eprintln!("mirrored {}", file.display());
                    }
                }
                // One unreadable message is no reason to abandon a long window.
                Err(e) => eprintln!("could not mirror {}: {e}", message.id),
            }
        }
    }

    // Bounded rather than pruned precisely: which ids are dropped doesn't
    // matter, only that the set can't grow for ever. Anything dropped is older
    // than `since` and won't come back.
    if seen.len() > REMEMBER {
        seen.clear();
    }
    Ok(printed)
}

// The sent folder, archived but not shown. Its own cursor and its own seen-set,
// so it can run at its own pace beside the folder being followed.
async fn mirror_sent(
    session: &Session,
    since: &mut DateTime<Utc>,
    until: Option<DateTime<Utc>>,
    seen: &mut HashSet<String>,
    sent_folder: Option<&str>,
    opts: &Opts,
) -> Result<usize, Error> {
    let Some(archive) = &opts.mirror else { return Ok(0) };
    let values = session.get_all(&messages_url("sentitems", *since, until), opts.verbose).await?;

    let mut written = 0;
    for value in &values {
        let Some(message) = message_from(value) else { continue };
        if !seen.insert(message.id.clone()) {
            continue;
        }
        if message.received > *since {
            *since = message.received;
        }
        match crate::message::render(session, &message.id, sent_folder, opts).await {
            Ok(rendered) => {
                let file =
                    archive.write(rendered.which, rendered.received, &message.id, &rendered.subject, &rendered.document)?;
                written += 1;
                if opts.verbose {
                    eprintln!("mirrored {}", file.display());
                }
            }
            Err(e) => eprintln!("could not mirror {}: {e}", message.id),
        }
    }
    if seen.len() > REMEMBER {
        seen.clear();
    }
    Ok(written)
}

fn messages_url(folder: &str, since: DateTime<Utc>, until: Option<DateTime<Utc>>) -> String {
    // "allmail" means every folder, which is what /me/messages is; anything
    // else names a folder, and Graph knows the well-known ones by name.
    let path = match folder {
        "allmail" | "all" => "/me/messages".to_string(),
        named => format!("/me/mailFolders/{}/messages", api::encode(named)),
    };
    // `lt` on the upper edge, so a window named by its end day excludes that
    // day: one chunk's start can be the next chunk's --before with no overlap.
    let filter = match until {
        Some(until) => format!("receivedDateTime ge {} and receivedDateTime lt {}", stamp(since), stamp(until)),
        None => format!("receivedDateTime ge {}", stamp(since)),
    };
    format!(
        "https://graph.microsoft.com/v1.0{path}?$select={SELECT}&$filter={}&$orderby=receivedDateTime%20asc&$top={PAGE}",
        api::encode(&filter)
    )
}

// Graph wants an ISO-8601 UTC instant, to the second.
fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn message_from(v: &Value) -> Option<Message> {
    let id = v.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let received = v
        .get("receivedDateTime")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|at| at.with_timezone(&Utc))?;
    // A sender is an address with an optional name; automated mail sometimes
    // has neither, and is still mail.
    let sender = v.get("from").and_then(|f| f.get("emailAddress"));
    let from = sender
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| sender.and_then(|a| a.get("address")).and_then(Value::as_str))
        .unwrap_or("?")
        .to_string();
    Some(Message {
        id: id.to_string(),
        received,
        subject: v
            .get("subject")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("(no subject)")
            .to_string(),
        from,
        unread: v.get("isRead").and_then(Value::as_bool) == Some(false),
    })
}

fn print_message(message: &Message, with_id: bool) {
    // Local time, because that's the clock the reader is on, even though Graph
    // speaks UTC.
    let when = message.received.with_timezone(&Local).format("%d.%m.%y %H:%M:%S");
    let mark = if message.unread { "*" } else { " " };
    // First, and unpainted, so a picker can hand it straight to show-message:
    // fzf's --with-nth=2.. hides it from the list and from the search while {1}
    // still resolves against the original line.
    let id = if with_id { format!("{} ", message.id) } else { String::new() };
    outln!(
        "{id}{}[{when}]{} {}{mark}{} {}{}{}: {}",
        c(DIM),
        c(RESET),
        c(YELLOW),
        c(RESET),
        c(CYAN),
        message.from,
        c(RESET),
        message.subject
    );
}

fn describe(window: TimeDelta) -> String {
    describe_std(Duration::from_secs(window.num_seconds().max(0) as u64))
}

fn describe_std(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        s if s % 86_400 == 0 && s >= 86_400 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 && s >= 3_600 => format!("{}h", s / 3_600),
        s if s % 60 == 0 && s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
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
    fn the_url_names_the_folder_and_filters_by_arrival() {
        let url = messages_url("inbox", at("2026-09-22T00:00:00Z"), None);
        assert!(url.starts_with("https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages?"), "got: {url}");
        assert!(url.contains("receivedDateTime%20ge%202026-09-22T00%3A00%3A00Z"), "got: {url}");
        assert!(url.contains("$orderby=receivedDateTime%20asc"), "got: {url}");
    }

    // Every folder, which is a different endpoint rather than a different filter.
    #[test]
    fn allmail_watches_every_folder() {
        let url = messages_url("allmail", at("2026-09-22T00:00:00Z"), None);
        assert!(url.starts_with("https://graph.microsoft.com/v1.0/me/messages?"), "got: {url}");
    }

    // A folder name is the one part of the URL that comes from configuration.
    #[test]
    fn a_folder_name_cannot_carry_anything_into_the_query() {
        let url = messages_url("../../me/messages?$top=1&x", at("2026-09-22T00:00:00Z"), None);
        assert!(!url.contains("../.."), "got: {url}");
        assert_eq!(url.matches('?').count(), 1, "only the query may start a query: {url}");
    }

    // A named end makes the window closed at both sides, which is what lets
    // chunks be chained without overlapping.
    #[test]
    fn a_named_end_closes_the_window_at_the_top() {
        let url = messages_url("inbox", at("2026-09-22T00:00:00Z"), Some(at("2026-09-29T00:00:00Z")));
        assert!(url.contains("receivedDateTime%20ge%202026-09-22T00%3A00%3A00Z"), "got: {url}");
        assert!(url.contains("and%20receivedDateTime%20lt%202026-09-29T00%3A00%3A00Z"), "got: {url}");
    }

    #[test]
    fn a_message_is_read_from_the_fields_graph_returns() {
        let m = message_from(&json!({
            "id": "AAMk", "receivedDateTime": "2026-09-29T12:22:07Z",
            "subject": "Rechnung", "isRead": false,
            "from": {"emailAddress": {"name": "Max Mustermann", "address": "max@example.com"}}
        }))
        .expect("has an id");

        assert_eq!((m.id.as_str(), m.subject.as_str(), m.from.as_str()), ("AAMk", "Rechnung", "Max Mustermann"));
        assert!(m.unread);
        assert_eq!(m.received, at("2026-09-29T12:22:07Z"));
    }

    #[test]
    fn a_sender_without_a_name_falls_back_to_its_address_and_then_to_nothing() {
        let addressed = message_from(&json!({
            "id": "1", "receivedDateTime": "2026-09-29T12:00:00Z",
            "from": {"emailAddress": {"address": "noreply@example.com"}}
        }))
        .unwrap();
        assert_eq!(addressed.from, "noreply@example.com");

        let anonymous = message_from(&json!({"id": "2", "receivedDateTime": "2026-09-29T12:00:00Z"})).unwrap();
        assert_eq!(anonymous.from, "?");
    }

    // A blank subject is normal, and an empty column reads as a bug.
    #[test]
    fn a_missing_subject_is_named_rather_than_left_blank() {
        let m = message_from(&json!({"id": "1", "receivedDateTime": "2026-09-29T12:00:00Z", "subject": "  "})).unwrap();
        assert_eq!(m.subject, "(no subject)");
    }

    // Without a timestamp there is no way to place it in the stream, and
    // nothing to move `since` to.
    #[test]
    fn a_message_without_an_id_or_a_timestamp_is_skipped() {
        assert!(message_from(&json!({"receivedDateTime": "2026-09-29T12:00:00Z"})).is_none());
        assert!(message_from(&json!({"id": "1"})).is_none());
        assert!(message_from(&json!({"id": "1", "receivedDateTime": "whenever"})).is_none());
    }

    #[test]
    fn a_window_is_described_in_the_largest_unit_that_fits() {
        assert_eq!(describe_std(Duration::from_secs(7 * 86_400)), "7d");
        assert_eq!(describe_std(Duration::from_secs(3_600)), "1h");
        assert_eq!(describe_std(Duration::from_secs(90)), "90s");
        assert_eq!(describe_std(Duration::from_secs(30)), "30s");
    }
}
