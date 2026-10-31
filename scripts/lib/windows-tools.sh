# Sourced. Finds a Windows tool on PATH or in its Program Files folder, since the LLVM and 7-Zip
# installers do not add themselves to PATH.
#   windows_tool <tool> <folder under Program Files>
windows_tool() {
  if command -v "$1" >/dev/null 2>&1; then
    printf '%s\n' "$1"
    return 0
  fi
  windows_tool_program_files=${PROGRAMFILES:-${ProgramFiles:-}}
  if [ -n "$windows_tool_program_files" ] && command -v cygpath >/dev/null 2>&1; then
    windows_tool_program_files=$(cygpath -u "$windows_tool_program_files")
  fi
  for windows_tool_candidate in "${windows_tool_program_files:-/c/Program Files}/$2/$1.exe" "/c/Program Files/$2/$1.exe"; do
    if [ -f "$windows_tool_candidate" ] && [ -x "$windows_tool_candidate" ]; then
      printf '%s\n' "$windows_tool_candidate"
      return 0
    fi
  done
  return 1
}
