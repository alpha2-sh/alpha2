// The native release archives decide their own FFmpeg: Windows ships only the bundled archive,
// and a bundle is smoke-tested unpacked with no loader environment, so a library reachable only
// through LD_LIBRARY_PATH or an absolute install name fails the release.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve, win32 } from "node:path";
import test from "node:test";

const root = resolve(import.meta.dirname, "..");
const buildRelease = join(root, "scripts/build-native-release.sh");
const smokeArchive = join(root, "release/ffmpeg-lgpl/smoke-archive.sh");

function run(command, args, options = {}) {
  return spawnSync(command, args, { encoding: "utf8", ...options });
}

const onWindows = process.platform === "win32";

/** Runs a shell script: Windows starts no file by its shebang, so there Git Bash's bash runs it. */
function script(path, args, options) {
  return onWindows
    ? run("bash", [path, ...args], options)
    : run(path, args, options);
}

function plan(mode, target) {
  return script(buildRelease, [mode, target], {
    env: { ...process.env, ALPHA2_RELEASE_PLAN: "1" },
  });
}

test("win-x64 releases the bundled archive only, the other targets thin and bundled", () => {
  const windows = plan("all", "win-x64");
  assert.equal(windows.status, 0, windows.stderr);
  assert.deepEqual(windows.stdout.trim().split("\n"), [
    "alpha2-0.1.0-win-x64-bundled.zip",
  ]);

  const thin = plan("thin", "win-x64");
  assert.notEqual(thin.status, 0, "a thin win-x64 archive is refused");
  assert.match(thin.stderr, /win-x64 has no thin archive/);

  for (const target of ["linux-x64", "linux-arm64", "macos-arm64"]) {
    const result = plan("all", target);
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(result.stdout.trim().split("\n"), [
      `alpha2-0.1.0-${target}-thin.tar.gz`,
      `alpha2-0.1.0-${target}-bundled.tar.gz`,
    ]);
  }
});

const cc = run("cc", ["--version"]).status === 0;
const elfFixtures =
  process.platform !== "linux"
    ? "the fixture bundles are ELF; smoke-archive.sh runs on each release runner"
    : !cc
      ? "no C compiler to build the fixture bundle"
      : false;

// A bundle shaped like the release one: executables with RUNPATH $ORIGIN/../lib and a
// libavdevice.so.62 needing libavfilter.so.11 with no RUNPATH of its own. `direct` makes the
// executables need libavfilter themselves, so their RUNPATH reaches it.
function elfBundle(scratch, target, { direct }) {
  const name = `alpha2-0.1.0-${target}-bundled`;
  const package_ = join(scratch, "build", name);
  const lib = join(package_, "lib");
  const bin = join(package_, "bin");
  mkdirSync(lib, { recursive: true });
  mkdirSync(bin, { recursive: true });
  const source = join(scratch, "src");
  mkdirSync(source, { recursive: true });
  writeFileSync(
    join(source, "filter.c"),
    "int fixture_filter(void) { return 11; }\n",
  );
  writeFileSync(
    join(source, "device.c"),
    "int fixture_filter(void);\nint fixture_device(void) { return fixture_filter() + 51; }\n",
  );
  writeFileSync(
    join(source, "main.c"),
    [
      "#include <stdio.h>",
      "#include <string.h>",
      "int fixture_device(void);",
      "int main(int argc, char **argv) {",
      "  const char *name = strrchr(argv[0], '/');",
      '  printf("usage: %s %d\\n", name ? name + 1 : argv[0], fixture_device());',
      "  return 2;",
      "}",
      "",
    ].join("\n"),
  );
  const compile = (args) => {
    const result = run("cc", args);
    assert.equal(result.status, 0, result.stderr);
  };
  compile([
    "-shared",
    "-fPIC",
    "-Wl,-soname,libavfilter.so.11",
    "-o",
    join(lib, "libavfilter.so.11"),
    join(source, "filter.c"),
  ]);
  compile([
    "-shared",
    "-fPIC",
    "-Wl,-soname,libavdevice.so.62",
    "-o",
    join(lib, "libavdevice.so.62"),
    join(source, "device.c"),
    `-L${lib}`,
    "-l:libavfilter.so.11",
  ]);
  compile([
    "-o",
    join(bin, "alpha2"),
    join(source, "main.c"),
    `-L${lib}`,
    "-l:libavdevice.so.62",
    ...(direct ? ["-Wl,--no-as-needed", "-l:libavfilter.so.11"] : []),
    "-Wl,--enable-new-dtags",
    "-Wl,-rpath,$ORIGIN/../lib",
  ]);
  copyFileSync(join(bin, "alpha2"), join(bin, "alpha2-host"));
  chmodSync(join(bin, "alpha2-host"), 0o755);
  const archive = join(scratch, `${name}.tar.gz`);
  const tar = run("tar", ["-czf", archive, "-C", join(scratch, "build"), name]);
  assert.equal(tar.status, 0, tar.stderr);
  return { archive, lib };
}

function withScratch(body) {
  const scratch = mkdtempSync(join(tmpdir(), "alpha2-archive-test-"));
  try {
    body(scratch);
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
}

test(
  "a self-contained bundle passes the smoke run",
  { skip: elfFixtures },
  () => {
    withScratch((scratch) => {
      const { archive } = elfBundle(scratch, "linux-x64", { direct: true });
      const result = run(smokeArchive, [archive, "linux-x64"]);
      assert.equal(result.status, 0, result.stderr);
      assert.match(
        result.stdout,
        /smoke-tested .* without a loader environment/,
      );
    });
  },
);

test(
  "a bundle whose RUNPATH misses a dependency fails the smoke run, however the caller's loader environment is set",
  { skip: elfFixtures },
  () => {
    withScratch((scratch) => {
      const { archive, lib } = elfBundle(scratch, "linux-x64", {
        direct: false,
      });
      // A loader environment pointing at the bundle's lib/ hides the gap when run directly.
      const hidden = run(
        join(scratch, "build/alpha2-0.1.0-linux-x64-bundled/bin/alpha2"),
        [],
        {
          env: { ...process.env, LD_LIBRARY_PATH: lib },
        },
      );
      assert.match(hidden.stdout, /usage: alpha2/);

      const result = run(smokeArchive, [archive, "linux-x64"], {
        env: { ...process.env, LD_LIBRARY_PATH: lib },
      });
      assert.notEqual(result.status, 0, result.stdout);
      for (const name of ["alpha2", "alpha2-host"]) {
        assert.match(
          result.stderr,
          new RegExp(
            `bin/${name} does not load from the unpacked archive:\n.*libavfilter\\.so\\.11: cannot open shared object file`,
          ),
        );
      }
    });
  },
);

// otool is macOS-only, so a stub stands in for it; the executables are the ELF fixture.
function stubOtool(scratch, installName) {
  const stubs = join(scratch, "stubs");
  mkdirSync(stubs, { recursive: true });
  const otool = join(stubs, "otool");
  writeFileSync(
    otool,
    [
      "#!/bin/sh",
      'echo "$2:"',
      `echo "	${installName} (compatibility version 62.0.0, current version 62.11.100)"`,
      'echo "	/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1351.0.0)"',
      "",
    ].join("\n"),
  );
  chmodSync(otool, 0o755);
  // The macOS branch unsets only DYLD_*, so a caller's LD_LIBRARY_PATH would replace the fixture's
  // libraries here.
  const env = { ...process.env, PATH: `${stubs}:${process.env.PATH}` };
  delete env.LD_LIBRARY_PATH;
  return env;
}

test(
  "a macOS bundle passes only when every FFmpeg install name is @rpath-relative",
  { skip: elfFixtures },
  () => {
    withScratch((scratch) => {
      const { archive } = elfBundle(scratch, "macos-arm64", { direct: true });
      const rpath = run(smokeArchive, [archive, "macos-arm64"], {
        env: stubOtool(scratch, "@rpath/libavcodec.62.dylib"),
      });
      assert.equal(rpath.status, 0, rpath.stderr);

      const homebrew = run(smokeArchive, [archive, "macos-arm64"], {
        env: stubOtool(
          scratch,
          "/opt/homebrew/opt/ffmpeg/lib/libavcodec.62.dylib",
        ),
      });
      assert.notEqual(homebrew.status, 0, homebrew.stdout);
      assert.match(
        homebrew.stderr,
        /bin\/alpha2 links FFmpeg by a path the archive does not control:\n\/opt\/homebrew\/opt\/ffmpeg\/lib\/libavcodec\.62\.dylib/,
      );
    });
  },
);

// A Git Bash stand-in on any POSIX host: `uname` reports MINGW64, `cygpath` passes paths through,
// and PATH holds only basic tools, so LLVM, 7-Zip, zip and unzip are wherever a test puts them.
// On Windows it is Git Bash itself with a minimal PATH. cargo is a stub.
function fakeGitBash(scratch) {
  const bin = join(scratch, "git-bash");
  mkdirSync(bin, { recursive: true });
  const programFiles = join(scratch, "Program Files");
  mkdirSync(programFiles, { recursive: true });
  // The crate license list needs node and cargo-about; only the version check runs here.
  executable(join(bin, "cargo"), "#!/bin/sh\nexit 0\n");
  if (onWindows) {
    const folders = new Set([bin, win32.dirname(process.execPath)]);
    for (const tool of ["bash", ...basicTools]) {
      const found = run("cygpath", ["-w", sh(`command -v ${tool}`)]);
      assert.equal(found.status, 0, `${tool}: ${found.stderr}`);
      folders.add(win32.dirname(found.stdout.trim()));
    }
    return {
      programFiles,
      shownProgramFiles: sh(`cygpath -u '${programFiles}'`),
      env: {
        PATH: [...folders].join(";"),
        PROGRAMFILES: programFiles,
        HOME: scratch,
      },
    };
  }
  for (const tool of basicTools) {
    const found = sh(`command -v ${tool}`);
    assert.ok(found, `${tool} is on this host`);
    symlinkSync(found, join(bin, tool));
  }
  symlinkSync(process.execPath, join(bin, "node"));
  executable(
    join(bin, "uname"),
    '#!/bin/sh\ncase "$1" in -s) echo MINGW64_NT-10.0-26100 ;; -m) echo x86_64 ;; esac\n',
  );
  executable(join(bin, "cygpath"), '#!/bin/sh\nshift\nprintf "%s\\n" "$1"\n');
  return {
    programFiles,
    shownProgramFiles: programFiles,
    env: { PATH: bin, PROGRAMFILES: programFiles, HOME: scratch },
  };
}

/** What `sh -c command` prints, trimmed. */
function sh(command) {
  return run("sh", ["-c", command]).stdout.trim();
}

// The tools the Windows packaging scripts use, which Git Bash has.
const basicTools = [
  "awk",
  "basename",
  "cat",
  "dirname",
  "find",
  "grep",
  "mkdir",
  "rm",
  "sed",
  "sort",
  "tr",
];

// Where packaging finds LLVM and 7-Zip on Windows regardless of PATH: a machine that has them
// there cannot run the tests that need them missing.
const installedLlvm =
  onWindows && existsSync("C:\\Program Files\\LLVM\\bin\\llvm-readobj.exe");
const installedSevenZip =
  onWindows && existsSync("C:\\Program Files\\7-Zip\\7z.exe");

function executable(path, text) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, text);
  chmodSync(path, 0o755);
}

// An llvm-readobj that lists one import for every file, as `llvm-readobj --coff-imports` prints it.
function fakeReadobj(path, imported) {
  executable(
    path,
    `#!/bin/sh\necho "Import {"\necho "  Name: ${imported}"\necho "}"\n`,
  );
}

test("Windows packaging finds LLVM and 7-Zip in their installers' default folders, as the SDK fetch does", () => {
  withScratch((scratch) => {
    const { programFiles, shownProgramFiles, env } = fakeGitBash(scratch);
    fakeReadobj(
      join(programFiles, "LLVM/bin/llvm-readobj.exe"),
      "KERNEL32.dll",
    );
    executable(join(programFiles, "7-Zip/7z.exe"), "#!/bin/sh\nexit 0\n");
    const result = script(buildRelease, ["all", "win-x64"], {
      env: { ...env, ALPHA2_RELEASE_CHECK_TOOLS: "1" },
    });
    assert.equal(result.status, 0, result.stderr);
    assert.match(
      result.stdout,
      new RegExp(
        `llvm-readobj: ${shownProgramFiles}/LLVM/bin/llvm-readobj\\.exe`,
      ),
    );
    assert.match(
      result.stdout,
      new RegExp(`7z: ${shownProgramFiles}/7-Zip/7z\\.exe`),
    );
  });
});

test(
  "Windows packaging without LLVM or 7-Zip stops before the SDK fetch and the build, naming both",
  {
    skip:
      (installedLlvm || installedSevenZip) &&
      "this machine has LLVM or 7-Zip in C:\\Program Files, where packaging finds them",
  },
  () => {
    withScratch((scratch) => {
      const { env } = fakeGitBash(scratch);
      const result = script(buildRelease, ["all", "win-x64"], { env });
      assert.notEqual(result.status, 0, result.stdout);
      assert.match(
        result.stderr,
        /^packaging win-x64 needs these tools .*: llvm-readobj\(LLVM\) 7z\(7-Zip\)$/m,
      );
      assert.doesNotMatch(result.stderr, /curl|fetch|cargo/);
    });
  },
);

test("the PE import check reads imports with an llvm-readobj found only in Program Files, and fails when it cannot read them", () => {
  withScratch((scratch) => {
    const { programFiles, env } = fakeGitBash(scratch);
    const dir = join(scratch, "bin");
    mkdirSync(dir);
    writeFileSync(join(dir, "alpha2.exe"), "");
    const checkPeImports = join(
      root,
      "release/ffmpeg-lgpl/check-pe-imports.sh",
    );

    // A machine with LLVM in C:\Program Files always has an llvm-readobj to find.
    if (!installedLlvm) {
      const absent = script(checkPeImports, [dir], { env });
      assert.notEqual(absent.status, 0, "no llvm-readobj is no pass");
      assert.match(absent.stderr, /needs llvm-readobj/);
    }

    const readobj = join(programFiles, "LLVM/bin/llvm-readobj.exe");
    fakeReadobj(readobj, "libwinpthread-1.dll");
    const foreign = script(checkPeImports, [dir], { env });
    assert.notEqual(foreign.status, 0, foreign.stdout);
    assert.match(
      foreign.stderr,
      /alpha2\.exe imports libwinpthread-1\.dll, which is neither shipped beside it nor part of Windows/,
    );

    fakeReadobj(readobj, "KERNEL32.dll");
    assert.equal(script(checkPeImports, [dir], { env }).status, 0);

    executable(
      readobj,
      '#!/bin/sh\necho "llvm-readobj: not a PE file" >&2\nexit 1\n',
    );
    const unreadable = script(checkPeImports, [dir], { env });
    assert.notEqual(unreadable.status, 0, "an unreadable file is no pass");
    assert.match(
      unreadable.stderr,
      /could not list the imports of alpha2\.exe/,
    );
  });
});

// build-native-release.sh in a scratch workspace on a macOS stand-in: `uname` reports Darwin arm64,
// the build steps are stubs, and install_name_tool, otool and codesign keep each binary's rpaths
// in a sidecar file, so packaging runs end to end on any POSIX host.
function fakeMacRelease(scratch) {
  const ws = join(scratch, "workspace");
  const stubs = join(scratch, "stubs");
  const sdk = join(scratch, "sdk");
  for (const dir of [
    "scripts/lib",
    "release/ffmpeg-lgpl",
    "target/release",
    "sdk/lib",
    "sdk/licenses/FFmpeg",
    "sdk/licenses/zlib",
    "sdk/licenses/dav1d",
  ])
    mkdirSync(join(dir.startsWith("sdk") ? scratch : ws, dir), {
      recursive: true,
    });
  for (const file of [
    "Cargo.toml",
    "LICENSE.md",
    "THIRD_PARTY_NOTICES.md",
    "release/ffmpeg-lgpl/manifest.toml",
    "scripts/build-native-release.sh",
    "scripts/lib/windows-tools.sh",
  ])
    copyFileSync(join(root, file), join(ws, file));
  chmodSync(join(ws, "scripts/build-native-release.sh"), 0o755);
  for (const name of ["FFmpeg", "zlib", "dav1d"])
    writeFileSync(join(sdk, "licenses", name, "LICENSE"), "");
  for (const lib of ["libavcodec.62.dylib", "libswscale.9.dylib"])
    writeFileSync(join(sdk, "lib", lib), "");
  executable(
    join(ws, "scripts/fetch-ffmpeg-sdk.sh"),
    `#!/bin/sh\necho "${sdk}"\n`,
  );
  executable(
    join(ws, "scripts/lib/check-rust-toolchain.sh"),
    '#!/bin/sh\nprintf "rustc 1.99.0 (stub)\\nrelease: 1.99.0\\n"\n',
  );
  // The crate license list: the packages it was asked for, in place of their licenses.
  writeFileSync(
    join(ws, "scripts/crate-licenses.mjs"),
    [
      'import { writeFileSync } from "node:fs";',
      "const args = process.argv.slice(2);",
      'writeFileSync(args[args.indexOf("--out") + 1], args.slice(4).join(" "));',
      "",
    ].join("\n"),
  );
  executable(
    join(ws, "release/ffmpeg-lgpl/verify-runtime.sh"),
    "#!/bin/sh\nexit 0\n",
  );
  executable(
    join(ws, "release/ffmpeg-lgpl/smoke-archive.sh"),
    "#!/bin/sh\nexit 0\n",
  );
  const report = /^report = "(.*)"$/m.exec(
    read(join(ws, "release/ffmpeg-lgpl/manifest.toml")).split(
      "[assets.macos-arm64]",
    )[1],
  )[1];
  const tag = /^release_tag = "(.*)"$/m.exec(
    read(join(ws, "release/ffmpeg-lgpl/manifest.toml")),
  )[1];
  mkdirSync(join(scratch, "cache", tag, "downloads"), { recursive: true });
  writeFileSync(join(scratch, "cache", tag, "downloads", report), "");

  executable(
    join(stubs, "uname"),
    '#!/bin/sh\ncase "$1" in -s) echo Darwin ;; -m) echo arm64 ;; esac\n',
  );
  // cargo build: the two executables, each linked with the rpaths RUSTFLAGS carries.
  executable(
    join(stubs, "cargo"),
    [
      "#!/bin/sh",
      'out="$(dirname "$(dirname "$0")")/workspace/target/release"',
      "for exe in alpha2 alpha2-host; do",
      '  printf "#!/bin/sh\\n" > "$out/$exe"',
      '  printf "%s\\n" "$RUSTFLAGS" | tr " " "\\n" | sed -n "s/^link-arg=-Wl,-rpath,//p" > "$out/$exe.rpaths"',
      "done",
      "",
    ].join("\n"),
  );
  executable(
    join(stubs, "install_name_tool"),
    '#!/bin/sh\n[ "$1" = -add_rpath ] || exit 1\nprintf "%s\\n" "$2" >> "$3.rpaths"\n',
  );
  executable(
    join(stubs, "otool"),
    [
      "#!/bin/sh",
      'if [ "$1" = -l ]; then',
      '  while read -r rpath; do printf "          cmd LC_RPATH\\n         path %s (offset 12)\\n" "$rpath"; done < "$2.rpaths"',
      "else",
      '  echo "$2:"',
      "fi",
      "",
    ].join("\n"),
  );
  executable(join(stubs, "codesign"), "#!/bin/sh\nexit 0\n");
  // cp does not carry the sidecar, so the package's copy starts from the linked rpaths.
  executable(
    join(stubs, "cp"),
    [
      "#!/bin/sh",
      '"$REAL_CP" "$@" || exit',
      "eval last=\\${$#}",
      'if [ $# -eq 2 ] && [ -f "$1.rpaths" ]; then "$REAL_CP" "$1.rpaths" "$last.rpaths"; fi',
      "",
    ].join("\n"),
  );
  const env = {
    ...process.env,
    PATH: `${stubs}:${process.env.PATH}`,
    REAL_CP: run("sh", ["-c", "command -v cp"]).stdout.trim(),
    ALPHA2_FFMPEG_CACHE: join(scratch, "cache"),
  };
  delete env.RUSTFLAGS;
  delete env.CARGO_BUILD_TARGET;
  const rpaths = (mode, exe) =>
    read(
      join(
        ws,
        "target/alpha2-native-release",
        `alpha2-0.1.0-macos-arm64-${mode}/bin/${exe}.rpaths`,
      ),
    )
      .trim()
      .split("\n");
  const toolchain = (mode) =>
    read(
      join(
        ws,
        "target/alpha2-native-release",
        `alpha2-0.1.0-macos-arm64-${mode}/compliance/TOOLCHAIN.txt`,
      ),
    );
  const crateLicenses = (mode) =>
    read(
      join(
        ws,
        "target/alpha2-native-release",
        `alpha2-0.1.0-macos-arm64-${mode}/licenses/rust-crate-licenses.md`,
      ),
    );
  return { ws, env, rpaths, toolchain, crateLicenses };
}

function read(path) {
  return readFileSync(path, "utf8");
}

test(
  "a thin macOS archive searches the system FFmpeg prefixes after its own lib/, and a bundled one only its own",
  { skip: process.platform === "win32" && "the packaging runs under POSIX sh" },
  () => {
    withScratch((scratch) => {
      const { ws, env, rpaths, toolchain, crateLicenses } =
        fakeMacRelease(scratch);
      const result = run(
        join(ws, "scripts/build-native-release.sh"),
        ["all", "macos-arm64"],
        {
          env,
        },
      );
      assert.equal(result.status, 0, result.stderr);
      for (const exe of ["alpha2", "alpha2-host"]) {
        assert.deepEqual(rpaths("thin", exe), [
          "@executable_path/../lib",
          "/opt/homebrew/lib",
          "/usr/local/lib",
          "/opt/local/lib",
        ]);
        assert.deepEqual(rpaths("bundled", exe), ["@executable_path/../lib"]);
      }
      for (const mode of ["thin", "bundled"]) {
        assert.equal(toolchain(mode), "rustc 1.99.0 (stub)\nrelease: 1.99.0\n");
        assert.equal(crateLicenses(mode), "alpha2-native alpha2-host");
      }
    });
  },
);
