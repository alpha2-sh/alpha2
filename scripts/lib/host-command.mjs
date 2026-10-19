/**
 * The command and arguments that run `host` with `args`. A Node script host runs under this Node,
 * since Windows starts no file by its shebang; any other host is spawned as it is.
 *
 * @param {string} host
 * @param {string[]} args
 * @returns {[string, string[]]}
 */
export function hostCommand(host, args) {
  return /\.[cm]?js$/i.test(host)
    ? [process.execPath, [host, ...args]]
    : [host, args];
}
