# mymail

Your Outlook mail in the terminal: load the last few days, then keep printing
subjects as they arrive.

It talks to Microsoft Graph with a bearer token taken from your own logged-in
browser session, rather than through a registered application. That makes it a
personal tool, not an integration: the token expires after a day at most, and
when it does, `mymail` says so and stops.

Sibling project to `myslackcli` and `myjira`: one static binary, credentials in
`~/.config/<tool>/.env`, plain lines on stdout that pipe into a fuzzy-finder.

## Setup

```bash
cargo build --release
mkdir -p ~/.config/mymail
cp .env.example ~/.config/mymail/.env
chmod 600 ~/.config/mymail/.env
# paste a token as described below, then:
./target/release/mymail 7
```

### Getting a token

The token has to be one for **Microsoft Graph** — audience
`https://graph.microsoft.com`. The Outlook web app is the wrong place to look:
it calls `outlook.office.com` with a token of its own, which Graph rejects.

The reliable way:

1. Open [Graph Explorer](https://developer.microsoft.com/graph/graph-explorer)
   and sign in with your work account.
2. **Grant `Mail.Read`**, and `Calendars.Read` for `show-events` — the
   **Modify permissions** tab next to the response,
   or just run `GET /me/messages` once and consent when asked. Without this step
   the token comes back carrying only `openid profile User.Read email`, and every
   request is refused; `mymail` then says which scopes it found.
3. Open the **Access token** tab and copy the token.
4. Paste it into `~/.config/mymail/.env` as `MAIL_TOKEN=…`. A `Bearer ` prefix
   copied along with it is stripped, so pasting a whole header value works too.

Out of a browser session instead: DevTools → **Network**, on a page that calls
Graph — `office.com` or Teams on the web do, Outlook doesn't — and copy the
`Authorization: Bearer …` value from any `graph.microsoft.com` request.

No quotes and no escaping needed: everything after the first `=` is the value,
verbatim to the end of the line.

It's a credential for your mailbox — don't share it, and keep the file out of
the checkout. How long it lasts depends on where it came from: a Graph Explorer
token measured 24 hours, one lifted from a page's own requests is usually about
an hour. When it runs out, every command says the token was rejected and stops,
rather than spinning.

## Usage

```bash
mymail 7                  # the last 7 days, then follow
mymail 2h                 # a shorter window, taken as written
mymail 7 --once           # print and exit, don't follow
mymail 7 --poll 2m        # check less often than the config says
mymail 7 --folder allmail # every folder, not just what arrived in the inbox
mymail show-events 7      # appointments in the next 7 days
mymail 7 --once --ids     # with each message's id in front
mymail show-message <id>  # one message, body and all
mymail 30 --mirror ~/mail # …and keep every one of them as Markdown
mymail --oldest --mirror ~/mail        # how far back the archive reaches
mymail 30 --before=2026-09-01 --mirror ~/mail   # an older stretch
mymail --help             # all flags
```

```
[29.09.26 12:22:07] * Max Mustermann: Rechnung September
[29.09.26 12:31:44]   noreply@example.com: Ihre Bestellung wurde versandt
```

Oldest first, so the backfill reads in the order things happened and new mail
arrives at the bottom. `*` marks unread. Ctrl-C stops the follow.

**Without `--once` it keeps running.** The window is loaded, and then it follows
— polling quietly for new mail until stopped. It says so when it gets there,
because the alternative is a program that has gone silent and looks like it has
hung. For loading an archive and getting the prompt back, `--once` is the flag:

```bash
mymail 5d --mirror ~/mail --once
```

### show-events

```
[29.09.26 09:00-09:30]   Daily Standup  (Microsoft Teams)
[30.09.26 all day    ]   Betriebsausflug
[01.10.26 14:00-15:00] ! Abstimmung (cancelled)
```

Occurrences, not series: a recurring meeting appears once per time it actually
falls in the window. From now rather than from midnight — "the next N days" is
forward-looking, and a meeting that finished this morning isn't what's being
asked about. `!` marks a cancelled occurrence, which is shown rather than hidden.

### show-message &lt;id&gt;

```
--------------------------------------------------------------------------

Action required: Prepare for Azure VM series retirements

From       Microsoft Azure <azure-noreply@microsoft.com>
To         Johannes Unger <unger@digitalklang.de>
Received   29.09.26 08:38:14

--------------------------------------------------------------------------

# Prepare for Azure VM series retirements and price changes.

Microsoft continues to invest significantly in Azure infrastructure …

◼
```

The body is HTML, and comes out as Markdown — so it goes straight into a
renderer:

```bash
mymail show-message <id> --no-color | glow -
```

### `--mirror <dir>`

Writes every message read as Markdown, so it can be grepped, diffed or opened
later without asking Microsoft again:

```
~/mail/inbox/2026/10/02/2026-10-02T1139-mcshfwaa-einladung-zur-coding-challenge.md
~/mail/outbox/2026/10/02/2026-10-02T1510-zlezdaaa-re-coding-challenge.md
```

Received and sent are kept apart at the top. **Sent mail is mirrored but not
printed** — this is a view of what is arriving, and one's own replies
interleaved with it is not what either is for; the run says how many it filed.
Which box a message goes in is asked of the message itself: a sent one carries
the sent folder's id in `parentFolderId` and nothing else does, which gets a mail
copied to oneself right where comparing the sender would not.

Year, month and day are strictly nested, and the file name starts with the time
— so the archive sorts itself and the oldest message is four directory reads
away rather than a scan. There is no database: the layout *is* the index. A week
level was considered and dropped, because an ISO week belongs to a year rather
than to a month, and a week directory under October can hold a message from the
28th of September — which would break exactly that property.

The name also carries the **end** of the message id. Not the start: an immutable
id is 68 characters of which the first 60 are identical for every message in a
mailbox — measured over 92 of them, the leading characters distinguish nothing
while the last six already tell all 92 apart. That makes the path a function of
the message, so opening the same mail twice rewrites one file instead of making
a second.

**Write-only, and it never deletes.** The listing carries no body, so each
mirrored message is one more request — which is why mirroring is opt-in rather
than something the listing does anyway.

### Reaching further back

`--before <date>` ends the window at local midnight of that day instead of at
now, and exits once it is loaded rather than following — so stretches of the
past can be pulled in chunks, and chained in a script:

```bash
mymail 30 --before=2026-09-01 --mirror ~/mail   # August
mymail 30 --before=2026-08-01 --mirror ~/mail   # July
```

The date is exclusive, so one chunk's start can be the next chunk's `--before`
with neither an overlap nor a gap. `--oldest` says how far the archive already
reaches — across both boxes, since they are pulled together — which is where the
next chunk should end:

```bash
$ mymail --oldest --mirror ~/mail
~/mail/inbox/2025/11/01/2025-11-01T1214-ruouxgaa-november-2025.md
```

### Identifying a message

`--ids` puts the message id first on every line, which is what `show-message`
takes. It's 68 characters, so it isn't meant to be typed — it's meant to be
piped:

```bash
mymail 7 --once --ids | fzf --ansi --with-nth=2..
```

`--with-nth=2..` hides the id from the list and from the search while `{1}`
still resolves against the original line, so a binding can hand it to
`show-message`.

That id is deliberately not the obvious one. A message's default Graph id is 152
characters **and changes when the message moves between folders** — the first
rule that files something away would break every id you had written down. Asking
for `Prefer: IdType="ImmutableId"` gives one that survives the move, and is
shorter as well. `mymail` asks for it on every request.

Pipes like the others:

```bash
mymail 7 --once | fzf --ansi
```

## Configuration

`~/.config/mymail/.env`:

| Key | What for |
| --- | --- |
| `MAIL_TOKEN` | the Graph bearer token |
| `POLL_INTERVAL` | how often to check for new mail; default `30s` |
| `MAIL_FOLDER` | which folder to follow; default `inbox`, or `allmail` |

`show-events` needs `Calendars.Read` in the token; the mail commands need
`Mail.Read`. A request refused for want of a scope says which one is missing and
which the token actually carries, since Graph itself only says "Access is
denied".

`--poll` and `--folder` override the last two for a single run. Real environment
variables win over the file.

## Why it polls

Because Microsoft offers a local program no way not to, and this was worth
checking properly:

- **Graph change notifications** are webhooks: Microsoft POSTs to a public
  HTTPS endpoint you operate. A program on your laptop hasn't got one.
- **Graph's socket.io endpoint** is the closest thing to the WebSocket
  `myslackcli` uses, but it exists for **drives and lists only** — not mail.
  Microsoft states outright that Graph has no equivalent to the old streaming
  model.
- **EWS streaming notifications** were that equivalent, and EWS is blocked in
  Exchange Online from **1 October 2026**.
- **IMAP IDLE** is genuine push, and Exchange Online advertises both `IDLE` and
  `AUTH=XOAUTH2`. But it needs a token for `outlook.office365.com`, which no
  browser session hands out — that road leads to a device-code login and a
  refresh token, which is a different tool from this one.

So it polls, and the interval is configuration rather than a constant. `30s` is
a reasonable default; a busy mailbox and a patient reader want different
numbers.

## fzf_example.sh

[`fzf_example.sh`](fzf_example.sh) puts the commands together as one picker:

```bash
X_MAIL_BIN=/path/to/mymail/target/release/mymail   # or leave it on PATH
. /path/to/mymail/fzf_example.sh
x_mail 7          # the last 7 days, then live — enter reads one in nvim,
                  # ctrl-r reloads the list
x_mail_once 7     # the same without waiting for more
x_mail_events 7   # the next 7 days in the calendar
x_mail_open <id>  # straight to one message
```

`x_mail_update_token <token>` puts a fresh token into the config, replacing
whatever `MAIL_TOKEN` held and leaving the rest of the file alone. A `Bearer `
prefix copied along with it is stripped, the replacement is one atomic rename so
an interruption can't leave a config without a token, and the new token is tried
once before the function returns — finding out now beats finding out in the
middle of a sync.

Sourced rather than run, works in bash and zsh, needs `fzf` and `nvim`.

The **list** runs with `--mirror`, so everything it shows is on disk by the time
it can be opened: the window it loaded, and everything that arrives while it
follows. Opening a message therefore writes nothing — it only renders what is
already there into a temporary file for nvim. Its stderr goes to a file of its
own and is folded into what nvim shows only when the render failed, so a refusal
is read rather than flashing past on a redraw.

`x_mail` streams into fzf and keeps streaming, so new mail appears in the list
as it arrives. The list comes from `FZF_DEFAULT_COMMAND` rather than a pipe, so
`ctrl-r` can run the very same command again — one definition for both, and no
way for a reload to show something other than what was first displayed. fzf ends
the old command before starting the next, so a reload replaces the stream rather
than leaving a second one polling alongside it. Enter uses fzf's `execute`, which hands the terminal to nvim and
takes it back — `become` would replace fzf and end the session on the first
message, and `execute-silent` would leave nvim without a terminal. The message
is rendered into a temporary file with stderr folded in, so a refusal is read in
nvim rather than flashing past on a redraw.

## Turning a mail into Markdown

A mail body is Outlook's HTML: tables for layout, conditional comments, tracking
pixels. Converting it is a solved problem, so it isn't solved again here —
[`htmd`](https://crates.io/crates/htmd) does it, which is turndown.js's approach
in Rust. `pandoc` would do as well, but depending on a binary being installed
makes the tool fragile in a way a crate doesn't.

Two things are worth knowing about the result:

- **`style`, `script`, `head` and `noscript` are skipped.** Outlook wraps its
  CSS in `<style><!-- … --></style>`, and a converter left to itself emits the
  comment's contents as text — several screens of it before the first word of
  the mail.
- **Images with no alt text are dropped.** In mail that's a spacer or a tracking
  pixel rather than content. One that carries alt text is saying something and
  stays.

Graph can also return the body as plain text
(`Prefer: outlook.body-content-type="text"`), which needs no conversion at all
but flattens links and structure away. `show-message` handles a text body if one
arrives, and asks for HTML.

## When a long run stops

Every request has a 30-second timeout, and a request that times out or never
connects is tried again up to three times before it gives up. This is the
difference between a run that stops and says so and one that appears to have
frozen: without a timeout a single stalled connection — a sleeping laptop, a VPN
that dropped, a proxy that accepted and then went quiet — leaves the program
waiting for ever with nothing on screen.

Throttling is a different thing and was looked for rather than assumed: 525
requests over 90 days of mail went through at about six a second with no
`429` at all. If Graph does throttle, it says so with a `Retry-After` and that
is waited out, reported as `Rate limited — waiting 10s (1/5)`.

## Tests

```bash
cargo test
```

The HTTP layer is tested against a canned TCP server: that the token goes out as
a bearer, the rate-limit retry and its budget, both shapes a dead token takes
(`401` and a redirect to a sign-in page), that a Graph error message is lifted
out of its envelope, and that pages are followed by their `@odata.nextLink`.
The rest covers the `.env` format, the window and interval parsing, the URL
construction and the message parsing.
