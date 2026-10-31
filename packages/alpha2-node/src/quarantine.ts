import { execFile } from "node:child_process";
import { existsSync, lstatSync, readdirSync, statSync } from "node:fs";
import { basename, dirname, join } from "node:path";

/**
 * How the quarantine check reads a file: whether macOS quarantined it (a browser or another
 * quarantine-aware program downloaded it), and whether Gatekeeper accepts it as `kind`, an
 * executable or a library. `accepted` is `undefined` when Gatekeeper did not answer.
 */
export interface QuarantineProbe {
  quarantined(path: string): Promise<boolean>;
  accepted(
    path: string,
    kind: "execute" | "library",
  ): Promise<boolean | undefined>;
}

/** How long Gatekeeper has to assess one file; it may ask Apple online. */
const ASSESSMENT_TIMEOUT_MS = 10_000;

/** The exit code of `command`, or `undefined` when it did not run or timed out. */
function exitCode(
  command: string,
  args: readonly string[],
  timeout: number,
): Promise<number | undefined> {
  return new Promise((resolve) => {
    execFile(command, args, { timeout }, (error) => {
      if (error === null) resolve(0);
      else if (typeof error.code === "number" && !error.killed)
        resolve(error.code);
      else resolve(undefined);
    });
  });
}

/** `xattr` and `spctl`, as macOS ships them. */
export const DEFAULT_QUARANTINE_PROBE: QuarantineProbe = {
  async quarantined(path) {
    return (
      (await exitCode(
        "/usr/bin/xattr",
        ["-p", "com.apple.quarantine", path],
        ASSESSMENT_TIMEOUT_MS,
      )) === 0
    );
  },
  async accepted(path, kind) {
    const code = await exitCode(
      "/usr/sbin/spctl",
      kind === "execute"
        ? ["--assess", "--type", "execute", path]
        : [
            "--assess",
            "--type",
            "open",
            "--context",
            "context:primary-signature",
            path,
          ],
      ASSESSMENT_TIMEOUT_MS,
    );
    return code === undefined ? undefined : code === 0;
  },
};

/**
 * The files `binary` needs that macOS checks when they are loaded: the executable, every library in
 * the archive's `lib/` beside its `bin/` (its files, not the links that name them again), and the
 * OpenH264 library `ALPHA2_OPENH264_LIBRARY` names.
 */
function filesOf(
  binary: string,
): { readonly path: string; readonly kind: "execute" | "library" }[] {
  const files: { path: string; kind: "execute" | "library" }[] = [
    { path: binary, kind: "execute" },
  ];
  const lib = join(dirname(binary), "..", "lib");
  try {
    for (const name of readdirSync(lib).sort())
      if (name.endsWith(".dylib") && lstatSync(join(lib, name)).isFile())
        files.push({ path: join(lib, name), kind: "library" });
  } catch {
    // No lib/ beside it: a host found on PATH or built from source.
  }
  const openh264 = process.env.ALPHA2_OPENH264_LIBRARY;
  if (openh264 && existsSync(openh264) && statSync(openh264).isFile())
    files.push({ path: openh264, kind: "library" });
  return files;
}

/**
 * Why macOS would hold `binary` on an approval prompt rather than run it: files it needs carry the
 * quarantine attribute and Gatekeeper does not accept them. The message names them and the remedy;
 * `undefined` when nothing is quarantined, Gatekeeper accepts what is, or Gatekeeper did not answer
 * (the start timeout is then what reports a held start).
 */
export async function quarantineRefusal(
  binary: string,
  probe: QuarantineProbe = DEFAULT_QUARANTINE_PROBE,
): Promise<string | undefined> {
  const files = filesOf(binary);
  const quarantined = await Promise.all(
    files.map(async (file) =>
      (await probe.quarantined(file.path)) ? file : undefined,
    ),
  );
  // Assessed at once: each may take a round trip to Apple.
  const verdicts = await Promise.all(
    quarantined.map((file) =>
      file === undefined
        ? Promise.resolve(undefined)
        : probe.accepted(file.path, file.kind),
    ),
  );
  const rejected = quarantined.flatMap((file, at) =>
    file !== undefined && verdicts[at] === false ? [file.path] : [],
  );
  if (rejected.length === 0) return undefined;
  const parent = dirname(binary);
  const root = basename(parent) === "bin" ? dirname(parent) : parent;
  const named = rejected.slice(0, 3).join(", ");
  const more = rejected.length > 3 ? ` and ${rejected.length - 3} more` : "";
  return (
    `macOS quarantined ${rejected.length} file(s) alpha2-host needs and Gatekeeper does not ` +
    `accept them, so it would hold alpha2-host on an approval prompt: ${named}${more}. Remove ` +
    `the attribute from the unpacked archive: xattr -dr com.apple.quarantine '${root}' (files ` +
    "downloaded with a browser carry it), or install with `npx @alpha2-sh/node install`, which " +
    "downloads without it."
  );
}
