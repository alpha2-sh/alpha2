#!/usr/bin/env bash
# Runs a command in the interactive console session of the user logged on at this Windows machine,
# for browser runs that need a real desktop (a session-0 or SSH process gets no GPU-backed browser
# window). Creates a one-shot interactive scheduled task, runs it, waits for its done marker, prints
# its log and deletes the task. The command runs from the repository root with the release gate's
# environment and the caller's ALPHA2_*, WGPU_* and PATH.
#   windows-console-run.sh <command...>
# Someone must be logged on at the console; with nobody there the task never starts.
set -uo pipefail

if [ $# -eq 0 ]; then
  echo "usage: windows-console-run.sh <command...>" >&2
  exit 2
fi
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) ;;
  *) echo "windows-console-run.sh runs on Windows (Git Bash) only" >&2; exit 2 ;;
esac

root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
task="alpha2-gate-$$"
trap 'MSYS2_ARG_CONV_EXCL="*" schtasks /Delete /TN "$task" /F > /dev/null 2>&1; rm -rf "$work"' EXIT
{
  echo "cd $(printf %q "$root") || exit 2"
  export -p | grep -E '^declare -x (ALPHA2_[A-Z0-9_]*|WGPU_[A-Z0-9_]*|PATH)='
  printf '%q ' "$@"
  echo "> $(printf %q "$work/log") 2>&1"
  echo "echo \$? > $(printf %q "$work/done")"
} > "$work/task.sh"

bash_exe=$(cygpath -w "$(command -v bash)")
script=$(cygpath -w "$work/task.sh")
# MSYS would turn the /Flags into paths.
MSYS2_ARG_CONV_EXCL="*" schtasks /Create /TN "$task" /TR "\"$bash_exe\" \"$script\"" \
  /SC ONCE /ST 23:59 /IT /F > /dev/null || exit 2
MSYS2_ARG_CONV_EXCL="*" schtasks /Run /TN "$task" > /dev/null || exit 2
# Three hours covers the longest browser pass; a task that never started (nobody at the console)
# ends here instead of waiting forever.
deadline=$((SECONDS + 10800))
while [ ! -f "$work/done" ]; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "FAIL the console task did not finish in three hours: is someone logged on at the console?"
    exit 1
  fi
  sleep 2
done
cat "$work/log"
exit "$(cat "$work/done")"
