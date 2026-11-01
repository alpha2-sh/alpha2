/**
 * A Node script host runs under this Node, since Windows ignores shebangs.
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
