# fzf_example.sh — the shell side of mymail, as one picker.
#
# Meant to be sourced, not run:
#
#     . /path/to/mymail/fzf_example.sh
#     x_mail 7          # the last 7 days, then live; enter reads one in nvim,
#                       # ctrl-r reloads the list
#     x_mail_once 7     # the same without waiting for more
#     x_mail_events 7   # the next 7 days in the calendar
#     x_mail_open <id>  # straight to one message
#     x_mail_update_token <token>   # put a fresh token in the config
#
# Works in bash and zsh. Needs fzf and nvim on PATH.
#
# This is a copy of what lives in the author's own shell setup, kept here so the
# README has something to point at. Set these before sourcing to override them —
# X_MAIL_BIN to an absolute target/release/mymail if it isn't on PATH:
X_MAIL_BIN=${X_MAIL_BIN:-mymail}
X_MAIL_ENV=${X_MAIL_ENV:-$HOME/.config/mymail/.env}
# Where a message read through the picker is kept. Both ways in open a message
# with --mirror, so reading one archives it.
X_MAIL_MIRROR=${X_MAIL_MIRROR:-$HOME/mails}

# ---------------------------------------------------------------------------
# x_mail — the inbox as a live picker: mymail streams into fzf and keeps
# streaming, so new mail appears in the list as it arrives. Enter reads the
# selected message in nvim and comes back to the list.
# ---------------------------------------------------------------------------

# --with-nth=2.. hides the message id from the list and from the search, while
# {1} still resolves against the original line — the same trick x_slack uses for
# its database ids. The id is 68 characters and nobody wants to see or type it.
#
# enter uses execute rather than become or execute-silent: become would replace
# fzf and end the session on the first message, and execute-silent would leave
# nvim with no terminal. execute hands the terminal over and takes it back.
#
# stderr goes into the file too, so a refusal — an expired token, most likely —
# is read in nvim rather than flashing past on a redraw.
#
# The list itself runs with --mirror, so everything it shows — the window it
# loaded and everything that arrives while it follows — is on disk by the time
# it can be opened. Opening one therefore writes nothing.
_x_mail_fzf()
{
  local header=$1
  shift
  # The header sits below the list, so the rule goes above it — "top" is the
  # line between the two. The keys belong with the list, they just shouldn't
  # read as one of its entries.
  fzf --ansi --no-sort --tac --style minimal --wrap=word --with-nth=2.. \
      --header "$header" --header-border=top "$@" \
      --bind "enter:execute(f=\$(mktemp --suffix=.md -t mymail-XXXXXX); \
\"$X_MAIL_BIN\" --env \"$X_MAIL_ENV\" show-message {1} --no-color \
> \$f 2>\$f.err || cat \$f.err >> \$f; nvim -R \$f; rm -f \$f \$f.err)"
}

# The same thing the picker does when a message is opened, for when the id is
# already known.
x_mail_open()
{
  local file rc
  if (( $# != 1 )); then
    echo 'usage: x_mail_open <message id>' >&2
    return 2
  fi
  file=$(mktemp --suffix=.md -t mymail-XXXXXX) || return 1
  # No --mirror here: the list a message was picked from archived it already,
  # and opening one is not the moment to make a request that writes.
  #
  # stderr to a file of its own, folded in only when the render failed, so a
  # refusal is read in nvim rather than flashing past on a redraw.
  "$X_MAIL_BIN" --env "$X_MAIL_ENV" show-message "$1" --no-color \
    > "$file" 2>"$file.err"
  rc=$?
  (( rc != 0 )) && cat "$file.err" >> "$file"
  nvim -R "$file"
  rm -f "$file" "$file.err"
  return $rc
}

# The last $1 days (7 by default), then live. Esc closes the picker, which
# closes the pipe, which stops the stream behind it.
# The list comes from FZF_DEFAULT_COMMAND rather than a pipe, so ctrl-r can run
# the very same command again — one definition for both, and no way for a reload
# to show something different from what was first displayed.
#
# mymail keeps running after it has printed the backfill, and fzf is happy to be
# fed by a command that never exits. It also ends that command before starting
# the next one, so a reload replaces the stream rather than adding a second one
# polling alongside it (checked).
x_mail()
{
  local rc cmd
  cmd="\"$X_MAIL_BIN\" --env \"$X_MAIL_ENV\" --mirror=\"$X_MAIL_MIRROR\" ${1:-7} --ids"
  FZF_DEFAULT_COMMAND=$cmd _x_mail_fzf '✉ · ctrl-r: reload' \
    --bind "ctrl-r:reload($cmd)"
  # Esc is how you leave a picker, not a failure — but fzf reports it as 130,
  # which a prompt that shows exit codes renders as one.
  rc=$?
  (( rc == 130 )) && return 0
  return $rc
}

# Everything already there, without waiting for more.
x_mail_once()
{
  local rc cmd
  cmd="\"$X_MAIL_BIN\" --env \"$X_MAIL_ENV\" --mirror=\"$X_MAIL_MIRROR\" ${1:-7} --once --ids"
  FZF_DEFAULT_COMMAND=$cmd _x_mail_fzf 'mail — enter: read · ctrl-r: reload · esc: quit' \
    --bind "ctrl-r:reload($cmd)"
  rc=$?
  (( rc == 130 )) && return 0
  return $rc
}

# Puts a fresh token into the config, replacing whatever MAIL_TOKEN held and
# leaving the rest of the file alone — a browser token lasts a day at most, so
# this is a thing one does often.
#
# A token copied out of an "Authorization: Bearer …" header brings the scheme
# with it; stripped here as well as by mymail itself, because the error it
# causes otherwise reads as an expired token rather than a malformed one.
x_mail_update_token()
{
  local token tmp check
  if (( $# != 1 )); then
    echo 'usage: x_mail_update_token <token>' >&2
    return 2
  fi
  token=${1#Bearer }
  token=${token#bearer }
  token=${token//[[:space:]]/}
  if [[ -z $token ]]; then
    echo 'x_mail_update_token: that is an empty token' >&2
    return 2
  fi
  if [[ ! -f $X_MAIL_ENV ]]; then
    echo "x_mail_update_token: no config at $X_MAIL_ENV" >&2
    return 1
  fi

  # Written beside the config rather than in /tmp, so replacing it is one atomic
  # rename on the same filesystem: an interruption can't leave a config without
  # a token in it.
  tmp=$(mktemp "$X_MAIL_ENV.XXXXXX") || return 1
  chmod 600 "$tmp"
  # The token goes through the environment, not through the awk program text:
  # it is base64 and carries characters awk would otherwise read as syntax.
  if ! MYMAIL_NEW_TOKEN=$token awk '
        /^MAIL_TOKEN=/ && !done { print "MAIL_TOKEN=" ENVIRON["MYMAIL_NEW_TOKEN"]; done = 1; next }
        { print }
        END { if (!done) print "MAIL_TOKEN=" ENVIRON["MYMAIL_NEW_TOKEN"] }
      ' "$X_MAIL_ENV" > "$tmp"; then
    rm -f "$tmp"
    return 1
  fi
  mv "$tmp" "$X_MAIL_ENV" || { rm -f "$tmp"; return 1; }

  # Tried once rather than taken on trust: the whole point of pasting a token is
  # that the old one stopped working, and finding out now beats finding out in
  # the middle of a sync.
  check=$(mktemp) || return 0
  if "$X_MAIL_BIN" --env "$X_MAIL_ENV" 1 --once > /dev/null 2>"$check"; then
    echo "token updated in $X_MAIL_ENV"
    rm -f "$check"
    return 0
  fi
  echo "token updated, but it was refused:" >&2
  cat "$check" >&2
  rm -f "$check"
  return 1
}

# The calendar reads as a whole rather than being searched one line at a time,
# so this one doesn't go through a picker.
x_mail_events()
{
  "$X_MAIL_BIN" --env "$X_MAIL_ENV" show-events "${1:-7}"
}
