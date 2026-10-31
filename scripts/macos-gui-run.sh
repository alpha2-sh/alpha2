#!/bin/sh
# Runs a command in the logged-in user's GUI session on macOS through a one-off LaunchAgent, writes
# its output to <log>, prints the log, and exits with the command's status:
#   macos-gui-run.sh <log> <command...>
# It is for callers without an Aqua session (`launchctl managername` is not `Aqua`): a CI runner
# service, or an SSH session on a machine where SSH has no access to the GUI session. Safari
# automation and a WebGPU adapter in Chrome or Firefox need the GUI session; the console must be
# unlocked for Safari.
# A LaunchAgent has no privacy (TCC) grants: a checkout under ~/Documents, ~/Desktop or ~/Downloads
# cannot be read from it and the command fails with status 126. Use a checkout elsewhere.
# The command runs in the current directory with this shell's PATH and ALPHA2_* variables.
set -eu

if [ "$(uname -s)" != Darwin ]; then
  echo "macos-gui-run.sh runs on macOS only" >&2
  exit 2
fi
if [ "$#" -lt 2 ]; then
  echo "usage: macos-gui-run.sh <log> <command...>" >&2
  exit 2
fi
log=$1
shift
case "$log" in /*) ;; *) log="$(pwd)/$log" ;; esac
case "$(pwd)/" in
  "$HOME"/Documents/*|"$HOME"/Desktop/*|"$HOME"/Downloads/*)
    echo "warning: $(pwd) is in a folder a LaunchAgent cannot read without a privacy grant; expect status 126" >&2 ;;
esac

uid=$(id -u)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/alpha2-gui-run-XXXXXX")
label="sh.alpha2.gui-run.$$"
plist="$scratch/$label.plist"
marker="$scratch/status"
loaded=0
cleanup() {
  if [ "$loaded" = 1 ]; then launchctl bootout "gui/$uid/$label" 2>/dev/null || true; fi
  rm -rf "$scratch"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

# Text as plist XML character data.
xml() {
  printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'
}

{
  printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?>' \
    '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
    '<plist version="1.0"><dict>'
  printf '<key>Label</key><string>%s</string>\n' "$label"
  printf '<key>WorkingDirectory</key><string>%s</string>\n' "$(xml "$(pwd)")"
  printf '%s\n' '<key>RunAtLoad</key><true/>' '<key>ProcessType</key><string>Interactive</string>'
  # The agent writes the command's status to the marker once the command ends.
  printf '%s\n' '<key>ProgramArguments</key><array>' '<string>/bin/sh</string>' '<string>-c</string>'
  printf '<string>%s</string>\n' "$(xml 'log=$1; marker=$2; shift 2; status=0; "$@" >"$log" 2>&1 || status=$?; echo "$status" >"$marker"')"
  printf '<string>macos-gui-run</string><string>%s</string><string>%s</string>\n' "$(xml "$log")" "$(xml "$marker")"
  for argument in "$@"; do printf '<string>%s</string>\n' "$(xml "$argument")"; done
  printf '%s\n' '</array>' '<key>EnvironmentVariables</key><dict>'
  printf '<key>PATH</key><string>%s</string>\n' "$(xml "$PATH")"
  printf '<key>HOME</key><string>%s</string>\n' "$(xml "$HOME")"
  for name in $(env | sed -n 's/^\(ALPHA2_[A-Za-z0-9_]*\)=.*/\1/p'); do
    eval "value=\${$name}"
    printf '<key>%s</key><string>%s</string>\n' "$name" "$(xml "$value")"
  done
  printf '%s\n' '</dict></dict></plist>'
} >"$plist"

launchctl bootstrap "gui/$uid" "$plist"
loaded=1
while [ ! -s "$marker" ]; do
  sleep 1
done
status=$(cat "$marker")
cat "$log"
exit "$status"
