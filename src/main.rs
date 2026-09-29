// println! panics when the reader goes away, and `mymail 7 | head` or `| fzf`
// is a normal way to use this — a panic with a backtrace is not the right
// answer to a pipe that has simply been closed. Defined before the modules so
// they can use it.
#[macro_export]
macro_rules! outln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = writeln!(std::io::stdout(), $($arg)*) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
            eprintln!("mymail: cannot write to stdout: {e}");
            std::process::exit(1);
        }
    }};
}

mod api;
mod config;
mod events;
mod mail;
mod message;

use std::sync::OnceLock;
use std::time::Duration;

use chrono::TimeDelta;

const USAGE: &str = "\
usage: mymail <days> [options]
       mymail show-events <days> [options]
       mymail show-message <id> [options]

  <days>                  how far back to load before following (e.g. 7), or
                          for show-events, how far ahead to look. A suffixed
                          value is taken as written: 2h, 30m, 3d.

options:
      --once              print what was loaded and exit instead of following
      --ids               put each message's id first, for show-message to take
                          (hidden from a picker with fzf --with-nth=2..)
      --poll <duration>   how often to check for new mail, overriding
                          POLL_INTERVAL in the config (e.g. 30s, 2m)
      --folder <name>     which folder to follow, overriding MAIL_FOLDER
                          (a well-known name like inbox, or allmail)
      --env <file>        read the configuration from this file
                          (default: ~/.config/mymail/.env)
      --no-color          plain output without ANSI colours
  -v, --verbose           report progress while running
  -h, --help              show this message
";

pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const CYAN: &str = "\x1b[36m";
pub const YELLOW: &str = "\x1b[33m";
pub const RESET: &str = "\x1b[0m";

static COLOR_ENABLED: OnceLock<bool> = OnceLock::new();

pub fn color_enabled() -> bool {
    *COLOR_ENABLED.get().unwrap_or(&false)
}

// Returns `code` if colour is enabled, otherwise "" — wrap every colour/reset
// constant with this instead of using them directly.
pub fn c(code: &'static str) -> &'static str {
    if color_enabled() { code } else { "" }
}

pub struct Opts {
    pub verbose: bool,
    pub once: bool,
    pub ids: bool,
}

type Error = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() {
    // Reported here rather than by returning Err from main: main's own error
    // reporting uses Debug formatting, which renders a message with quotes and
    // any usage text with literal escapes.
    if let Err(e) = run().await {
        eprintln!("mymail: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Error> {
    let mut verbose = false;
    let mut no_color = false;
    let mut once = false;
    let mut ids = false;
    let mut env_path: Option<String> = None;
    let mut poll: Option<String> = None;
    let mut folder: Option<String> = None;
    let mut words: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-v" | "--verbose" => verbose = true,
            "--no-color" => no_color = true,
            "--once" => once = true,
            "--ids" => ids = true,
            "--poll" => poll = Some(args.next().ok_or("missing value for --poll (e.g. --poll=30s)")?),
            other if other.starts_with("--poll=") => poll = Some(other.trim_start_matches("--poll=").to_string()),
            "--folder" => folder = Some(args.next().ok_or("missing value for --folder (e.g. --folder=inbox)")?),
            other if other.starts_with("--folder=") => {
                folder = Some(other.trim_start_matches("--folder=").to_string())
            }
            "--env" => env_path = Some(args.next().ok_or("missing value for --env (e.g. --env=/path/.env)")?),
            other if other.starts_with("--env=") => env_path = Some(other.trim_start_matches("--env=").to_string()),
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            // Silently ignoring an unknown flag hides typos: a mistyped --poll
            // would mean a run that looks fine and polls at the wrong rate.
            other if other.starts_with('-') => {
                eprintln!("unknown argument: {other}\n");
                eprint!("{USAGE}");
                std::process::exit(2);
            }
            other => words.push(other.to_string()),
        }
    }
    COLOR_ENABLED.set(!no_color).ok();

    if words.is_empty() {
        print!("{USAGE}");
        return Ok(());
    }
    // The mail window is positional and has no verb of its own, so a command
    // is recognised by being one rather than by coming first.
    let (command, argument) = match words[0].as_str() {
        "show-events" => ("show-events", words.get(1)),
        "show-message" => ("show-message", words.get(1)),
        _ => ("mail", words.first()),
    };
    let expected = if command == "mail" { 1 } else { 2 };
    if words.len() > expected {
        return Err(format!("unexpected argument: {}\n\n{USAGE}", words[expected]).into());
    }

    let mut config = config::load(env_path.as_deref())?;
    if let Some(text) = &poll {
        config.poll = config::parse_duration(text)
            .ok_or_else(|| format!("--poll={text} is not a duration — try 30s, 2m, 1h"))?;
    }
    if let Some(name) = folder {
        config.folder = name.trim().to_lowercase();
    }

    let opts = Opts { verbose, once, ids };
    match command {
        "show-message" => {
            let id = argument.ok_or("show-message needs a message id — `mymail 7 --ids` prints them")?;
            message::show(&config, id, &opts).await
        }
        other => {
            let argument = argument.ok_or("show-events needs a number of days, e.g. mymail show-events 7")?;
            let window = parse_window(argument)
                .ok_or_else(|| format!("{argument:?} is not a number of days — try 7, or 2h"))?;
            match other {
                "show-events" => events::run(&config, window, &opts).await,
                _ => mail::run(&config, window, &opts).await,
            }
        }
    }
}

// A bare number is days, because that's what the argument is for. A suffixed
// one is taken at its word, so `mymail 2h` needn't be spelled as a fraction.
fn parse_window(text: &str) -> Option<TimeDelta> {
    let text = text.trim();
    let duration = if text.chars().all(|ch| ch.is_ascii_digit()) {
        let days: u64 = text.parse().ok()?;
        (days > 0).then(|| Duration::from_secs(days * 24 * 60 * 60))?
    } else {
        config::parse_duration(text)?
    };
    TimeDelta::from_std(duration).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_number_is_days() {
        assert_eq!(parse_window("7"), Some(TimeDelta::days(7)));
        assert_eq!(parse_window("1"), Some(TimeDelta::days(1)));
    }

    // Spelling out a shorter window shouldn't mean working out a fraction.
    #[test]
    fn a_suffixed_window_is_taken_as_written() {
        assert_eq!(parse_window("2h"), Some(TimeDelta::hours(2)));
        assert_eq!(parse_window("30m"), Some(TimeDelta::minutes(30)));
        assert_eq!(parse_window("3d"), Some(TimeDelta::days(3)));
    }

    #[test]
    fn a_window_that_would_load_nothing_or_isnt_one_is_refused() {
        for bad in ["0", "0h", "", "-1", "soon", "7 days"] {
            assert_eq!(parse_window(bad), None, "{bad:?} should be refused");
        }
    }
}
