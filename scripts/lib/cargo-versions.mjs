// The Cargo manifest lines `version:set` owns, and the rewrite it applies to them.

const workspaceVersion = /^(version = )"[^"]*"$/;
const siblingRequirement =
  /^(alpha2[\w-]* = \{ version = )"=?[^"]*"(?=, path = )/;

/** What `version:set` writes in the workspace `Cargo.toml`, by section. */
export const workspaceSections = (version) => ({
  "workspace.package": [workspaceVersion, version],
  // Siblings require each other exactly, as only one set of versions is built and tested together.
  "workspace.dependencies": [siblingRequirement, `=${version}`],
});

/** What `version:set` writes in `plugins/Cargo.toml`, by section. */
export const pluginsSections = (version) => ({
  "workspace.package": [workspaceVersion, version],
});

/**
 * Rewrites, in each `[section]` of `sections`, the lines its pattern matches to `$1"<value>"`.
 * `sections` maps a section name to `[pattern, value]`. Returns the text and the number of lines.
 */
export function setCargoVersions(text, sections) {
  let section = "";
  let lines = 0;
  const rewritten = text
    .split("\n")
    .map((line) => {
      section = line.match(/^\[([^\]]+)\]$/)?.[1] ?? section;
      const [pattern, value] = sections[section] ?? [];
      if (!pattern?.test(line)) return line;
      lines += 1;
      return line.replace(pattern, `$1"${value}"`);
    })
    .join("\n");
  return { text: rewritten, lines };
}
