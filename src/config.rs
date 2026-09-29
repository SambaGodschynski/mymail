use std::collections::HashMap;
use std::time::Duration;

use crate::Error;

pub const DEFAULT_POLL: Duration = Duration::from_secs(30);

pub struct Config {
    pub token: String,
    pub poll: Duration,
    pub folder: String,
}

// Written out rather than derived: the token is a live credential, and a
// derived Debug would print it in full the first time a Config ended up in a
// log line, an error or a panic message.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("token", &format_args!("<{} bytes, redacted>", self.token.len()))
            .field("poll", &self.poll)
            .field("folder", &self.folder)
            .finish()
    }
}

pub fn load(env_path: Option<&str>) -> Result<Config, Error> {
    let mut vars = match env_path {
        // Named explicitly, so a path that doesn't load is an error rather than
        // something to shrug off.
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("could not read {path}: {e}"))?;
            parse_env(&text)
        }
        // Deliberately not the working directory: this is a credential, and a
        // file next to the source is one stray `git add -f` from being public.
        None => match default_env_path() {
            Some(path) if path.exists() => {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("could not read {}: {e}", path.display()))?;
                parse_env(&text)
            }
            _ => HashMap::new(),
        },
    };
    for (key, value) in std::env::vars() {
        if key.starts_with("MAIL_") || key == "POLL_INTERVAL" {
            vars.insert(key, value);
        }
    }

    let token = vars.get("MAIL_TOKEN").map(|t| strip_scheme(t)).unwrap_or_default();
    if token.is_empty() {
        return Err("MAIL_TOKEN not set — put it in ~/.config/mymail/.env (see .env.example)".into());
    }
    let poll = match vars.get("POLL_INTERVAL").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(text) => parse_duration(text)
            .ok_or_else(|| format!("POLL_INTERVAL={text:?} is not a duration — try 30s, 2m, 1h"))?,
        None => DEFAULT_POLL,
    };
    let folder = vars
        .get("MAIL_FOLDER")
        .map(|f| f.trim().to_lowercase())
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| "inbox".to_string());

    Ok(Config { token, poll, folder })
}

// A token is copied out of an `Authorization: Bearer …` header as often as not,
// and the scheme comes along with it. Sending it on produces
// "Bearer Bearer eyJ…", which Graph rejects as an expired token — a diagnosis
// that sends you looking in entirely the wrong place.
fn strip_scheme(token: &str) -> String {
    let token = token.trim();
    let without = token
        .strip_prefix("Bearer ")
        .or_else(|| token.strip_prefix("bearer "))
        .unwrap_or(token);
    without.trim().to_string()
}

// The scopes a token carries, read out of its own payload.
//
// A bearer token is a JWT: three base64url parts, and the middle one is public
// JSON. Nothing is verified here — the point isn't to trust the token, it's to
// explain a refusal that Graph answers with nothing more than "Access is
// denied", which sends you looking at the mailbox rather than at the consent
// you never granted.
pub fn scopes(token: &str) -> Option<Vec<String>> {
    let payload = token.split('.').nth(1)?;
    let json = base64url(payload)?;
    let claims: serde_json::Value = serde_json::from_slice(&json).ok()?;
    let scp = claims.get("scp")?.as_str()?;
    Some(scp.split_whitespace().map(str::to_string).collect())
}

// Just enough base64url for a JWT payload: no padding, "-_" for "+/".
fn base64url(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bits = 0u32;
    let mut have = 0u32;
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for byte in text.bytes() {
        let value = ALPHABET.iter().position(|c| *c == byte)? as u32;
        bits = (bits << 6) | value;
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
        }
    }
    Some(out)
}

// Graph refuses a token without the right scope with nothing but "Access is
// denied", which reads as a mailbox or calendar problem. The token says what it
// can actually do, so it gets to answer.
//
// `needed` is matched by family — a command wanting Mail.Read is satisfied by
// Mail.ReadWrite, and asking for the exact spelling would send people to grant
// a permission they already have.
pub fn explain(e: crate::Error, token: &str, needed: &str) -> crate::Error {
    if !e.to_string().contains("403") {
        return e;
    }
    let family = needed.split_once('.').map(|(family, _)| family).unwrap_or(needed);
    match scopes(token) {
        Some(scopes) if !scopes.iter().any(|s| s.starts_with(&format!("{family}."))) => format!(
            "{e}\n       this token carries no {family} scope, only: {}\n       grant {needed} where you got it and copy a fresh one",
            scopes.join(" ")
        )
        .into(),
        _ => e,
    }
}

pub fn default_env_path() -> Option<std::path::PathBuf> {
    let config_home = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(config_home.join("mymail").join(".env"))
}

// Everything after the first "=" is the value, verbatim to the end of the line.
// The same rule as myjira's, and for the same reason: a pasted credential
// shouldn't need quoting or escaping to survive.
fn parse_env(text: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if !key.is_empty() {
            vars.insert(key.to_string(), unquote(value.trim()).to_string());
        }
    }
    vars
}

// Only a matching pair counts: a lone quote is likelier to be part of a value
// than an attempt at quoting.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(quote).and_then(|v| v.strip_suffix(quote)) {
            return inner;
        }
    }
    value
}

// "45s", "30m", "2h", "3d", and a bare number as seconds.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let (digits, factor) = match text.chars().last()? {
        's' => (&text[..text.len() - 1], 1),
        'm' => (&text[..text.len() - 1], 60),
        'h' => (&text[..text.len() - 1], 60 * 60),
        'd' => (&text[..text.len() - 1], 60 * 60 * 24),
        _ => (text, 1),
    };
    let amount: u64 = digits.trim().parse().ok()?;
    // Zero would be a busy loop against someone else's server.
    (amount > 0).then(|| Duration::from_secs(amount * factor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_read_with_or_without_a_unit() {
        assert_eq!(parse_duration("45s"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("3d"), Some(Duration::from_secs(259_200)));
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
    }

    // A zero interval would hammer Graph as fast as the network allows.
    #[test]
    fn a_zero_or_nonsense_duration_is_refused() {
        for bad in ["0s", "0", "", "s", "-5s", "abc", "1x"] {
            assert_eq!(parse_duration(bad), None, "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_token_survives_the_env_file_verbatim() {
        let token = "eyJ0eXAiOiJKV1QiLCJub25jZSI6Il9-abc.def-ghi_jkl.mno";
        let parsed = parse_env(&format!("MAIL_TOKEN={token}\nPOLL_INTERVAL=15s\n"));

        assert_eq!(parsed.get("MAIL_TOKEN").map(String::as_str), Some(token));
        assert_eq!(parsed.get("POLL_INTERVAL").map(String::as_str), Some("15s"));
    }

    // What actually happens when someone copies the header rather than the
    // token: the value arrives as "Bearer eyJ…" and every request 401s.
    // Built here rather than pasted in, so the test carries no real token.
    fn jwt(claims: &str) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in claims.as_bytes().chunks(3) {
            let mut buffer = [0u8; 3];
            buffer[..chunk.len()].copy_from_slice(chunk);
            let n = u32::from_be_bytes([0, buffer[0], buffer[1], buffer[2]]);
            for i in 0..(chunk.len() + 1) {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        format!("header.{out}.signature")
    }

    #[test]
    fn the_scopes_a_token_carries_are_readable_from_it() {
        let token = jwt(r#"{"aud":"00000003-0000-0000-c000-000000000000","scp":"openid profile Mail.Read"}"#);
        assert_eq!(
            scopes(&token),
            Some(vec!["openid".to_string(), "profile".to_string(), "Mail.Read".to_string()])
        );
    }

    #[test]
    fn a_refusal_names_the_scope_that_is_missing() {
        let token = jwt(r#"{"scp":"openid profile Mail.Read"}"#);
        let refused = crate::Error::from("ErrorAccessDenied (HTTP 403)");

        let explained = explain(refused, &token, "Calendars.Read").to_string();
        assert!(explained.contains("no Calendars scope"), "got: {explained}");
        assert!(explained.contains("grant Calendars.Read"), "got: {explained}");
    }

    // A command wanting Mail.Read is satisfied by Mail.ReadWrite; sending
    // someone to grant what they already have would be worse than saying
    // nothing.
    #[test]
    fn a_wider_scope_in_the_same_family_counts() {
        let token = jwt(r#"{"scp":"Mail.ReadWrite"}"#);
        let refused = crate::Error::from("ErrorAccessDenied (HTTP 403)");

        assert_eq!(explain(refused, &token, "Mail.Read").to_string(), "ErrorAccessDenied (HTTP 403)");
    }

    // Anything but a 403 is about something else entirely.
    #[test]
    fn another_failure_is_passed_through_untouched() {
        let token = jwt(r#"{"scp":"openid"}"#);
        let other = crate::Error::from("rate limited");
        assert_eq!(explain(other, &token, "Mail.Read").to_string(), "rate limited");
    }

    #[test]
    fn something_that_is_not_a_token_yields_no_scopes() {
        assert_eq!(scopes("not-a-jwt"), None);
        assert_eq!(scopes("a.!!!.c"), None);
        assert_eq!(scopes(&jwt(r#"{"aud":"x"}"#)), None);
    }

    #[test]
    fn a_pasted_authorization_scheme_is_stripped() {
        assert_eq!(strip_scheme("Bearer eyJ0eXAi"), "eyJ0eXAi");
        assert_eq!(strip_scheme("bearer eyJ0eXAi"), "eyJ0eXAi");
        assert_eq!(strip_scheme("  Bearer   eyJ0eXAi  "), "eyJ0eXAi");
        assert_eq!(strip_scheme("eyJ0eXAi"), "eyJ0eXAi");
    }

    // A token beginning with those letters is still a token.
    #[test]
    fn only_the_scheme_and_its_space_are_stripped() {
        assert_eq!(strip_scheme("Bearereyj"), "Bearereyj");
    }

    #[test]
    fn comments_blank_lines_quotes_and_an_export_prefix_are_handled() {
        let parsed = parse_env("# a comment\n\nexport MAIL_FOLDER=\"inbox\"\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.get("MAIL_FOLDER").map(String::as_str), Some("inbox"));
    }

    // Guards the redaction above: this is the type that carries the token.
    #[test]
    fn debug_output_does_not_contain_the_token() {
        let config = Config {
            token: "eyJ-secret".to_string(),
            poll: DEFAULT_POLL,
            folder: "inbox".to_string(),
        };

        let printed = format!("{config:?}");
        assert!(!printed.contains("secret"), "the token must not be printable: {printed}");
        assert!(printed.contains("redacted"), "got: {printed}");
    }
}
