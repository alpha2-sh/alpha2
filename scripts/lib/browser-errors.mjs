/**
 * A browser that never reached its page. A run stops on one, since every later page would fail the
 * same way after the full start timeout.
 */
export class BrowserStartError extends Error {}
