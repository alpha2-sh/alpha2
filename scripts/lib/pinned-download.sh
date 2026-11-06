# Sourced. Helpers the pinned downloads share: host target detection, checksummed downloads,
# a cross-process lock, and archive extraction.

detect_target() {
  os=$(uname -s)
  machine=$(uname -m)
  case "$os-$machine" in
    Linux-x86_64) echo linux-x64 ;;
    Linux-aarch64|Linux-arm64) echo linux-arm64 ;;
    Darwin-arm64) echo macos-arm64 ;;
    Darwin-x86_64) echo macos-x64 ;;
    MINGW*-x86_64|MSYS*-x86_64|CYGWIN*-x86_64) echo win-x64 ;;
    *) echo "unsupported host: $os-$machine" >&2; exit 1 ;;
  esac
}

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum < "$1" | awk '{print $1}'
  else
    shasum -a 256 < "$1" | awk '{print $1}'
  fi
}

native_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s\n' "$1"
  fi
}

# A `mkdir` lock (macOS has no flock) holding the owner's PID so a dead owner's lock is taken over.
acquire_lock() {
  lock=$1
  waiting=
  until mkdir "$lock" 2>/dev/null; do
    owner=$(cat "$lock/pid" 2>/dev/null || true)
    if lock_is_abandoned "$lock" "$owner"; then
      # Move aside first so only one fetch takes it over; restore it if another took it meanwhile.
      if mv "$lock" "$lock.abandoned-$$" 2>/dev/null; then
        if [ "$(cat "$lock.abandoned-$$/pid" 2>/dev/null || true)" = "$owner" ] || [ -e "$lock" ]; then
          rm -rf "$lock.abandoned-$$"
        else
          mv "$lock.abandoned-$$" "$lock"
        fi
      fi
      continue
    fi
    if [ -z "$waiting" ]; then
      echo "waiting for another fetch holding $lock" >&2
      waiting=1
    fi
    sleep 1
  done
  echo "$$" > "$lock/pid"
}

# The owner writes its PID right after mkdir, so an ownerless lock older than a minute is stale.
lock_is_abandoned() {
  if [ -n "$2" ]; then
    ! ps -p "$2" >/dev/null 2>&1
  else
    [ -n "$(find "$1" -prune -mmin +1 2>/dev/null)" ]
  fi
}

download() {
  url=$1
  output=$2
  expected=$3
  if [ -f "$output" ] && [ "$(hash_file "$output")" = "$expected" ]; then
    return
  fi
  partial="$output.partial.$$"
  trap 'rm -f "$partial"' EXIT HUP INT TERM
  echo "downloading $(basename "$output")" >&2
  # A stall counts as a timeout, which --retry retries.
  curl --fail --location --retry 3 --connect-timeout 30 \
    --speed-limit 1024 --speed-time 60 --output "$partial" "$url"
  actual=$(hash_file "$partial")
  if [ "$actual" != "$expected" ]; then
    echo "checksum mismatch for $(basename "$output"): expected $expected, got $actual" >&2
    exit 1
  fi
  mv "$partial" "$output"
  trap - EXIT HUP INT TERM
}

# Extracts <archive> into <directory>, with unzip or 7-Zip for a zip (Git Bash on Windows has no
# unzip).
extract_archive() {
  case "$1" in
    *.zip)
      if command -v unzip >/dev/null 2>&1; then
        unzip -q "$1" -d "$2"
      else
        seven_zip=$(windows_tool 7z 7-Zip) || {
          echo "extracting $(basename "$1") needs unzip or 7z (7-Zip): on PATH or in Program Files/7-Zip" >&2
          exit 1
        }
        "$seven_zip" x -y "-o$(native_path "$2")" "$(native_path "$1")" >/dev/null
      fi
      ;;
    *.tar.gz) tar -xzf "$1" -C "$2" ;;
    *) echo "unsupported archive: $1" >&2; exit 1 ;;
  esac
}
