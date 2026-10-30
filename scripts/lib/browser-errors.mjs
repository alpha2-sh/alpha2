// Errors the browser runners share.

/**
 * A browser that never reached its page: it could not be spawned, exited, never opened its
 * DevTools endpoint or WebDriver session, or the connection to it failed. A run that sees one
 * stops, since every later page would fail the same way after the full start timeout.
 */
export class BrowserStartError extends Error {}
