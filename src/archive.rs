//! The mirror: rendered mail on disk, laid out so the oldest one can be found
//! without an index.
//!
//! ```text
//! <root>/inbox/2026/10/02/2026-10-02T1422-mcshfwaa-rechnung-september.md
//! <root>/outbox/2026/10/02/2026-10-02T1510-zlfqtwaa-re-rechnung-september.md
//! ```
//!
//! Received and sent are kept apart at the top, because reading a year of one's
//! own replies interleaved with everything that arrived is not what either is
//! for.
//!
//! Year, month and day are strictly nested, which is what makes `--oldest` four
//! directory reads: the smallest year holds the smallest month holds the
//! smallest day holds the oldest file. A week would break that — an ISO week
//! belongs to a year rather than to a month, so a week directory under October
//! can hold a message from the 28th of September, and the first directory would
//! no longer be the one with the first message.
//!
//! Write-only, and it never deletes. A path is derived from the message, so
//! opening the same mail twice rewrites one file rather than making a second.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};

use crate::Error;

// The *end* of the id, not the start. An immutable id is 68 characters of which
// the first 60 are the same for every message in a mailbox — measured across 92
// of them, the leading characters distinguish nothing at all while the last six
// already tell all 92 apart. Eight leaves headroom.
const ID_TAIL: usize = 8;
const MAX_SLUG: usize = 60;

pub const INBOX: &str = "inbox";
pub const OUTBOX: &str = "outbox";

// Which box a message belongs in, asked of the message itself: a sent message
// carries the sent folder's id in parentFolderId, and nothing else does. The
// alternative — comparing the sender to the signed-in account — gets a mail
// copied to oneself wrong.
//
// Without a sent folder id to compare against (the lookup can fail, and it is
// not worth failing a whole run over) everything reads as received, which is
// where it would have gone before this existed.
pub fn box_of(parent_folder_id: Option<&str>, sent_folder_id: Option<&str>) -> &'static str {
    match (parent_folder_id, sent_folder_id) {
        (Some(parent), Some(sent)) if parent == sent => OUTBOX,
        _ => INBOX,
    }
}

// The last ID_TAIL characters, or all of them if it is shorter.
fn tail(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    chars[chars.len().saturating_sub(ID_TAIL)..].iter().collect()
}

pub struct Archive {
    root: PathBuf,
}

impl Archive {
    pub fn new(root: &str) -> Self {
        Self { root: PathBuf::from(root) }
    }

    // Local time, not UTC: the day a mail belongs to is the day it arrived
    // where the reader was.
    pub fn path_for(&self, which: &str, received: DateTime<Local>, id: &str, subject: &str) -> PathBuf {
        let name = format!(
            "{}-{}-{}.md",
            received.format("%Y-%m-%dT%H%M"),
            slug(&tail(id)),
            slug(subject)
        );
        self.root
            .join(which)
            .join(received.format("%Y").to_string())
            .join(received.format("%m").to_string())
            .join(received.format("%d").to_string())
            .join(name)
    }

    pub fn write(
        &self,
        which: &str,
        received: DateTime<Local>,
        id: &str,
        subject: &str,
        body: &str,
    ) -> Result<PathBuf, Error> {
        let file = self.path_for(which, received, id, subject);
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        }
        let body = if body.ends_with('\n') { body.to_string() } else { format!("{body}\n") };
        std::fs::write(&file, body).map_err(|e| format!("could not write {}: {e}", file.display()))?;
        Ok(file)
    }

    // The oldest message in the archive, across both boxes — which is the date
    // the next chunk should end at, and they are pulled together so they move
    // together.
    pub fn oldest(&self) -> Result<Option<PathBuf>, Error> {
        let mut best: Option<(String, PathBuf)> = None;
        for which in [INBOX, OUTBOX] {
            let Some(path) = self.oldest_in(&self.root.join(which))? else { continue };
            // By file name, not by path: the name starts with the timestamp, so
            // it orders across boxes as well as within one.
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if best.as_ref().is_none_or(|(best, _)| name < *best) {
                best = Some((name, path));
            }
        }
        Ok(best.map(|(_, path)| path))
    }

    // Four directory reads rather than a walk of everything: at each level the
    // smallest name is the one to descend into, which only holds because the
    // levels are year, month and day.
    fn oldest_in(&self, box_root: &Path) -> Result<Option<PathBuf>, Error> {
        let mut at = box_root.to_path_buf();
        for _ in 0..3 {
            match smallest(&at, true)? {
                Some(next) => at = next,
                None => return Ok(None),
            }
        }
        smallest(&at, false)
    }
}

// The smallest entry by name, looking only at directories or only at files.
// Names that aren't what this layout writes are skipped rather than tripped
// over: a stray README in the archive shouldn't become its oldest message.
fn smallest(dir: &Path, want_dir: bool) -> Result<Option<PathBuf>, Error> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // An archive that was never written to is empty, not broken.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("could not read {}: {e}", dir.display()).into()),
    };

    let mut best: Option<(String, PathBuf)> = None;
    for entry in entries {
        let entry = entry.map_err(|e| format!("could not read {}: {e}", dir.display()))?;
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir != want_dir {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let plausible = if want_dir {
            name.chars().all(|c| c.is_ascii_digit())
        } else {
            name.ends_with(".md")
        };
        if !plausible {
            continue;
        }
        if best.as_ref().is_none_or(|(best, _)| name < *best) {
            best = Some((name, entry.path()));
        }
    }
    Ok(best.map(|(_, path)| path))
}

// One safe path segment from arbitrary text: a subject is free text, and this
// is the one place it becomes a path.
fn slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    let mut slug: String = trimmed.chars().take(MAX_SLUG).collect();
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() { "unnamed".to_string() } else { slug }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, min, 0).single().expect("a real local time")
    }

    #[test]
    fn the_path_carries_the_date_the_id_and_the_subject() {
        let archive = Archive::new("/tmp/mail");
        let path = archive.path_for(INBOX, at(2026, 10, 2, 14, 22), "AAkALgAAAAAAHzK7qega", "Rechnung September");

        assert_eq!(
            path.to_str().unwrap(),
            "/tmp/mail/inbox/2026/10/02/2026-10-02T1422-hzk7qega-rechnung-september.md"
        );
    }

    // Opening the same mail twice must rewrite one file, not make a second.
    #[test]
    fn the_path_is_the_same_every_time_for_the_same_message() {
        let archive = Archive::new("/tmp/mail");
        let first = archive.path_for(INBOX, at(2026, 10, 2, 14, 22), "ABC12345xyz", "Betreff");
        let second = archive.path_for(INBOX, at(2026, 10, 2, 14, 22), "ABC12345xyz", "Betreff");
        assert_eq!(first, second);
    }

    // Two mails in the same minute are told apart by the id, not by luck — and
    // real ids differ only at the end, which is the whole reason the tail is
    // what goes in the name.
    #[test]
    fn two_messages_in_one_minute_do_not_share_a_path() {
        let archive = Archive::new("/tmp/mail");
        let common = "AAkALgAAAAAAHYQDEapmEc2byACqAC-EWg0AL-bijhXnv0eWUSrM0LHx_QAH";
        let a = archive.path_for(INBOX, at(2026, 10, 2, 14, 22), &format!("{common}zK7qegAA"), "Gleicher Betreff");
        let b = archive.path_for(INBOX, at(2026, 10, 2, 14, 22), &format!("{common}zLDYdAAA"), "Gleicher Betreff");
        assert_ne!(a, b, "ids that share their first 60 characters must still differ");
    }

    #[test]
    fn a_short_id_is_used_whole_rather_than_panicking() {
        assert_eq!(tail("abc"), "abc");
        assert_eq!(tail(""), "");
    }

    #[test]
    fn a_subject_cannot_escape_the_archive() {
        assert_eq!(slug("../../etc/passwd"), "etc-passwd");
        assert_eq!(slug("Re: [EXTERN] Angebot?"), "re-extern-angebot");
        assert_eq!(slug(""), "unnamed");
        assert_eq!(slug("///"), "unnamed");
        for bad in ["a/b", "a\\b", "a\0b", "a\nb"] {
            assert!(!slug(bad).contains(['/', '\\', '\0', '\n']), "{bad:?} -> {}", slug(bad));
        }
    }

    #[test]
    fn a_very_long_subject_is_cut_without_a_dangling_separator() {
        let long = format!("{} - tail", "x".repeat(MAX_SLUG - 2));
        assert!(slug(&long).len() <= MAX_SLUG);
        assert!(!slug(&long).ends_with('-'), "got: {}", slug(&long));
    }

    #[test]
    fn the_oldest_is_found_by_descending_the_smallest_name_at_each_level() {
        let root = std::env::temp_dir().join(format!("mymail-archive-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let archive = Archive::new(root.to_str().unwrap());

        // Deliberately written newest first, so order of creation can't be what
        // the answer depends on.
        for (y, m, d, h) in [(2026, 10, 2, 14), (2026, 10, 1, 9), (2025, 12, 31, 23), (2026, 1, 5, 8)] {
            archive.write(INBOX, at(y, m, d, h, 0), "ID123456", "Betreff", "body").unwrap();
        }

        let oldest = archive.oldest().unwrap().expect("something is in there");
        assert!(oldest.ends_with("inbox/2025/12/31/2025-12-31T2300-id123456-betreff.md"), "got: {}", oldest.display());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_message_knows_its_own_box() {
        assert_eq!(box_of(Some("SENT"), Some("SENT")), OUTBOX);
        assert_eq!(box_of(Some("OTHER"), Some("SENT")), INBOX);
        // No sent folder to compare against, or no parent at all: received,
        // which is where everything went before boxes existed.
        assert_eq!(box_of(Some("SENT"), None), INBOX);
        assert_eq!(box_of(None, Some("SENT")), INBOX);
    }

    // The two boxes are pulled together, so the archive reaches back as far as
    // the older of them.
    #[test]
    fn the_oldest_is_the_oldest_of_both_boxes() {
        let root = std::env::temp_dir().join(format!("mymail-boxes-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let archive = Archive::new(root.to_str().unwrap());

        archive.write(INBOX, at(2026, 3, 2, 9, 0), "IDAAAAAA", "Empfangen", "body").unwrap();
        archive.write(OUTBOX, at(2026, 1, 5, 8, 0), "IDBBBBBB", "Gesendet", "body").unwrap();

        let oldest = archive.oldest().unwrap().expect("something is in there");
        assert!(oldest.ends_with("outbox/2026/01/05/2026-01-05T0800-idbbbbbb-gesendet.md"), "got: {}", oldest.display());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_archive_that_does_not_exist_yet_has_no_oldest_rather_than_failing() {
        let archive = Archive::new("/tmp/mymail-archive-that-is-not-there");
        assert_eq!(archive.oldest().unwrap(), None);
    }

    // A stray file in the archive shouldn't be mistaken for the oldest message.
    #[test]
    fn entries_that_are_not_part_of_the_layout_are_skipped() {
        let root = std::env::temp_dir().join(format!("mymail-stray-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let archive = Archive::new(root.to_str().unwrap());
        archive.write(INBOX, at(2026, 5, 4, 12, 0), "ID123456", "Betreff", "body").unwrap();
        std::fs::write(root.join(INBOX).join("README.md"), "not a message").unwrap();
        std::fs::create_dir_all(root.join(INBOX).join("notes")).unwrap();

        let oldest = archive.oldest().unwrap().expect("the real message");
        assert!(oldest.ends_with("inbox/2026/05/04/2026-05-04T1200-id123456-betreff.md"), "got: {}", oldest.display());

        std::fs::remove_dir_all(&root).ok();
    }
}
