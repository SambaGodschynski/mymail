use chrono::{DateTime, Local, Utc};
use serde_json::Value;

use crate::api::{self, Session};
use crate::config::Config;
use crate::{BOLD, DIM, Error, Opts, RESET};

const SELECT: &str =
    "subject,from,toRecipients,ccRecipients,receivedDateTime,hasAttachments,body,parentFolderId";
const WIDTH: usize = 74;
// Long enough to see who was in the conversation, short enough that a mail to
// the whole company doesn't push the message off the screen.
const MAX_RECIPIENTS: usize = 5;

pub async fn show(config: &Config, id: &str, opts: &Opts) -> Result<(), Error> {
    let id = id.trim();
    if id.is_empty() {
        return Err("show-message needs a message id — `mymail 7 --ids` prints them".into());
    }
    let session = Session::new(&api::client(), config.token.clone());
    let message = fetch(&session, id, opts)
        .await
        .map_err(|e| crate::config::explain(e, &config.token, "Mail.Read"))?;

    if let Some(archive) = &opts.mirror {
        let sent = sent_folder_id(&session, opts).await;
        let which = crate::archive::box_of(parent_folder(&message), sent.as_deref());
        let file = archive.write(which, received_at(&message), id, subject(&message), &document(&message, false)?)?;
        eprintln!("mirrored {}", file.display());
    }
    outln!("{}", document(&message, crate::color_enabled())?);
    Ok(())
}

async fn fetch(session: &Session, id: &str, opts: &Opts) -> Result<Value, Error> {
    let url = message_url(id);
    if opts.verbose {
        eprintln!("GET {url}");
    }
    session.get(&url).await
}

// One message, fetched and rendered plain, for the listing to archive. The same
// two steps `show` takes, so the file is the same either way.
pub struct Rendered {
    pub received: DateTime<Local>,
    pub subject: String,
    pub document: String,
    pub which: &'static str,
}

pub async fn render(
    session: &Session,
    id: &str,
    sent_folder_id: Option<&str>,
    opts: &Opts,
) -> Result<Rendered, Error> {
    let message = fetch(session, id, opts).await?;
    Ok(Rendered {
        received: received_at(&message),
        subject: subject(&message).to_string(),
        document: document(&message, false)?,
        which: crate::archive::box_of(parent_folder(&message), sent_folder_id),
    })
}

fn parent_folder(message: &Value) -> Option<&str> {
    message.get("parentFolderId").and_then(Value::as_str).filter(|s| !s.is_empty())
}

// The sent folder's id, asked for once so every message can say whether it is
// one of its own. Not fatal if it cannot be had: everything then reads as
// received, which is where it all went before boxes existed.
pub async fn sent_folder_id(session: &Session, opts: &Opts) -> Option<String> {
    let url = "https://graph.microsoft.com/v1.0/me/mailFolders/sentitems?$select=id";
    if opts.verbose {
        eprintln!("GET {url}");
    }
    match session.get(url).await {
        Ok(folder) => folder.get("id").and_then(Value::as_str).map(str::to_string),
        Err(e) => {
            eprintln!("could not tell which folder is Sent ({e}) — filing everything as received");
            None
        }
    }
}

fn subject(message: &Value) -> &str {
    message
        .get("subject")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("(no subject)")
}

// Falls back to now rather than refusing: a message with no timestamp still has
// to land somewhere, and today is the least surprising place.
fn received_at(message: &Value) -> DateTime<Local> {
    message
        .get("receivedDateTime")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|at| at.with_timezone(&Local))
        .unwrap_or_else(Local::now)
}

fn message_url(id: &str) -> String {
    // The id goes in the path, and an immutable id is base64url — but encoding
    // it costs nothing and means a default id, which isn't, works too.
    format!("https://graph.microsoft.com/v1.0/me/messages/{}?$select={SELECT}", api::encode(id))
}

fn document(message: &Value, colour: bool) -> Result<String, Error> {
    let paint = |code: &'static str| if colour { code } else { "" };
    let subject = subject(message);

    let bar = "-".repeat(WIDTH);
    let mut out = String::new();
    // A rule is only a rule in Markdown with a blank line before it: dashes
    // straight under a line of text turn that line into a heading instead.
    let rule = |out: &mut String| {
        out.push_str(&format!("\n{}{bar}{}\n\n", paint(DIM), paint(RESET)));
    };

    out.push_str(&format!("{}{bar}{}\n\n", paint(DIM), paint(RESET)));
    // Two trailing spaces: consecutive lines are one paragraph in Markdown, so
    // without a hard break a renderer runs the whole header onto one line.
    out.push_str(&format!("{}{subject}{}  \n\n", paint(BOLD), paint(RESET)));

    let mut field = |label: &str, value: String| {
        if !value.is_empty() {
            out.push_str(&format!("{}{label:<10}{} {value}  \n", paint(DIM), paint(RESET)));
        }
    };
    field("From", address(message.get("from")));
    field("To", addresses(message.get("toRecipients")));
    field("Cc", addresses(message.get("ccRecipients")));
    field("Received", received(message));
    if message.get("hasAttachments").and_then(Value::as_bool) == Some(true) {
        field("", "(has attachments)".to_string());
    }

    rule(&mut out);
    out.push_str(&body(message)?);
    // Its own paragraph, or a renderer folds it into the last line of the body.
    out.push_str("\n\n◼");
    Ok(out)
}

// The body arrives as HTML — Outlook's own, which means tables for layout,
// conditional comments and tracking pixels. htmd is left to deal with that; all
// this adds is tidying, because a newsletter converts into a great deal of
// vertical whitespace.
fn body(message: &Value) -> Result<String, Error> {
    let body = message.get("body");
    let content = body.and_then(|b| b.get("content")).and_then(Value::as_str).unwrap_or("");
    if content.trim().is_empty() {
        return Ok("(no body)".to_string());
    }
    // Graph can be asked for a plain-text body instead, and then there is
    // nothing to convert.
    if body.and_then(|b| b.get("contentType")).and_then(Value::as_str) == Some("text") {
        return Ok(tidy(content));
    }
    // Outlook wraps its CSS in <style><!-- … --></style>, and a converter left
    // to itself emits the comment's contents as text — several screens of it
    // before the first word of the mail.
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "head", "noscript"])
        .build();
    let markdown = converter.convert(content).map_err(|e| format!("could not convert the body: {e}"))?;
    Ok(tidy(&markdown))
}

// Collapses the runs of blank lines that laid-out HTML turns into, and drops
// trailing spaces that aren't a Markdown line break.
fn tidy(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut blanks = 0;
    for line in text.lines() {
        let trimmed = line.trim_end();
        // A tracking pixel is an image with nothing to say about itself. One
        // that carries alt text is telling you something and stays.
        if is_blind_image(trimmed) {
            continue;
        }
        if trimmed.is_empty() {
            blanks += 1;
            // One blank line separates paragraphs; more is just the layout
            // showing through.
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        lines.push(trimmed.to_string());
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

// `![](…)` on a line of its own, optionally linked: an image with no alt text,
// which in mail is a spacer or a tracking pixel rather than content.
fn is_blind_image(line: &str) -> bool {
    let line = line.trim();
    // rsplit, because a linked image contains a "](" of its own before the
    // one that closes the link.
    let inner = line.strip_prefix('[').and_then(|l| l.rsplit_once("](")).map(|(inner, _)| inner).unwrap_or(line);
    inner.starts_with("![](") && line.ends_with(')')
}

fn address(from: Option<&Value>) -> String {
    let email = from.and_then(|f| f.get("emailAddress"));
    let name = email.and_then(|e| e.get("name")).and_then(Value::as_str).filter(|s| !s.is_empty());
    let addr = email.and_then(|e| e.get("address")).and_then(Value::as_str).filter(|s| !s.is_empty());
    match (name, addr) {
        // Both, because the display name is what you recognise and the address
        // is what tells you it's really them.
        (Some(name), Some(addr)) if name != addr => format!("{name} <{addr}>"),
        (Some(name), _) => name.to_string(),
        (_, Some(addr)) => addr.to_string(),
        _ => String::new(),
    }
}

fn addresses(list: Option<&Value>) -> String {
    let Some(list) = list.and_then(Value::as_array) else { return String::new() };
    let names: Vec<String> = list.iter().map(|r| address(Some(r))).filter(|a| !a.is_empty()).collect();
    if names.len() <= MAX_RECIPIENTS {
        return names.join(", ");
    }
    format!("{}, (+{} more)", names[..MAX_RECIPIENTS].join(", "), names.len() - MAX_RECIPIENTS)
}

fn received(message: &Value) -> String {
    message
        .get("receivedDateTime")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|at| at.with_timezone(&Utc).with_timezone(&Local).format("%d.%m.%y %H:%M:%S").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_id_is_encoded_into_the_path() {
        let url = message_url("AAkALgAAAAA-_x=");
        assert!(url.starts_with("https://graph.microsoft.com/v1.0/me/messages/"), "got: {url}");
        // base64url survives as itself; padding becomes an escape.
        assert!(url.contains("AAkALgAAAAA-_x%3D"), "got: {url}");
    }

    #[test]
    fn a_sender_is_shown_by_name_and_address() {
        let from = json!({"emailAddress": {"name": "Max Mustermann", "address": "max@example.com"}});
        assert_eq!(address(Some(&from)), "Max Mustermann <max@example.com>");
    }

    // Automated mail often sets the name to the address; printing it twice
    // reads as a bug.
    #[test]
    fn a_name_equal_to_the_address_is_not_repeated() {
        let from = json!({"emailAddress": {"name": "noreply@x.de", "address": "noreply@x.de"}});
        assert_eq!(address(Some(&from)), "noreply@x.de");
        assert_eq!(address(None), "");
    }

    #[test]
    fn a_long_recipient_list_is_cut_and_counted() {
        let many: Vec<Value> = (0..9)
            .map(|i| json!({"emailAddress": {"address": format!("a{i}@x.de")}}))
            .collect();
        let line = addresses(Some(&json!(many)));

        assert!(line.starts_with("a0@x.de, a1@x.de"), "got: {line}");
        assert!(line.ends_with("(+4 more)"), "got: {line}");
    }

    #[test]
    fn an_html_body_becomes_markdown() {
        let message = json!({"body": {"contentType": "html",
            "content": "<h1>Titel</h1><p>Ein <b>fetter</b> Satz mit <a href=\"https://x.de\">Link</a>.</p>"}});

        let markdown = body(&message).unwrap();
        assert!(markdown.contains("# Titel"), "got: {markdown}");
        assert!(markdown.contains("**fetter**"), "got: {markdown}");
        assert!(markdown.contains("[Link](https://x.de)"), "got: {markdown}");
    }

    // Graph can be asked for text instead, and then there is nothing to do.
    #[test]
    fn a_text_body_is_passed_through_rather_than_converted() {
        let message = json!({"body": {"contentType": "text", "content": "plain <not html>"}});
        assert_eq!(body(&message).unwrap(), "plain <not html>");
    }

    #[test]
    fn an_empty_body_is_named_rather_than_left_blank() {
        assert_eq!(body(&json!({"body": {"contentType": "html", "content": "  "}})).unwrap(), "(no body)");
        assert_eq!(body(&json!({})).unwrap(), "(no body)");
    }

    // Laid-out HTML converts into a great deal of vertical whitespace.
    #[test]
    fn runs_of_blank_lines_collapse_to_one() {
        assert_eq!(tidy("a\n\n\n\n\nb"), "a\n\nb");
        assert_eq!(tidy("a   \nb\t"), "a\nb");
        assert_eq!(tidy("a\n\n\n"), "a");
    }

    // The bug this fixes: Outlook's CSS arrived as several screens of text
    // ahead of the first word of the mail.
    #[test]
    fn styles_and_scripts_do_not_reach_the_output() {
        let message = json!({"body": {"contentType": "html", "content":
            "<html><head><style><!-- .x {color:red} --></style></head>             <body><script>alert(1)</script><p>Der Text.</p></body></html>"}});

        let markdown = body(&message).unwrap();
        assert_eq!(markdown, "Der Text.");
    }

    #[test]
    fn an_image_without_alt_text_is_dropped_and_one_with_it_is_kept() {
        assert!(is_blind_image("![](https://x.de/pixel.gif)"));
        assert!(is_blind_image("[![](https://x.de/pixel.gif)](https://x.de)"));
        assert!(!is_blind_image("![Microsoft Azure](https://x.de/logo.png)"));
        assert!(!is_blind_image("Ein Satz mit ![](x) mittendrin"));

        assert_eq!(tidy("![](https://x.de/p.gif)\nText"), "Text");
    }

    #[test]
    fn the_document_carries_the_header_the_body_and_an_end() {
        let message = json!({
            "subject": "Rechnung", "receivedDateTime": "2026-09-29T12:22:07Z",
            "from": {"emailAddress": {"name": "Max", "address": "max@x.de"}},
            "toRecipients": [{"emailAddress": {"address": "me@x.de"}}],
            "hasAttachments": true,
            "body": {"contentType": "html", "content": "<p>Anbei.</p>"}
        });

        let doc = document(&message, false).unwrap();
        assert!(doc.contains("Rechnung"), "got:\n{doc}");
        assert!(doc.contains("From       Max <max@x.de>"), "got:\n{doc}");
        assert!(doc.contains("To         me@x.de"), "got:\n{doc}");
        assert!(doc.contains("(has attachments)"), "got:\n{doc}");
        assert!(doc.contains("Anbei."), "got:\n{doc}");
        assert!(doc.ends_with("◼"), "got:\n{doc}");
        // A rule needs a blank line above it or it makes a heading of the line
        // before.
        assert!(doc.contains(&format!("\n\n{}\n\n", "-".repeat(WIDTH))), "got:\n{doc}");
    }
}
