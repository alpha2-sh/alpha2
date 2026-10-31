import http, { type IncomingMessage } from "node:http";
import https from "node:https";
import tls from "node:tls";

/**
 * How long a download may go without receiving data. Exported for this package's tests, which
 * shorten it; the package's entry point does not export it.
 */
export const HTTP_SETTINGS = { idleTimeoutMs: 60_000 };

const MAX_REDIRECTS = 5;

// `proxyEnv` (Node 22.21 and 24.5) postdates the pinned @types/node.
interface ProxyAgentOptions extends https.AgentOptions {
  readonly proxyEnv?: NodeJS.ProcessEnv;
}

let certificates: string[] | undefined;

/**
 * Node's bundled roots (with `NODE_EXTRA_CA_CERTS`) and the operating system's store: the
 * Keychain, the Windows store, or OpenSSL's default file and directory on Linux, where
 * `SSL_CERT_FILE` and `SSL_CERT_DIR` move them. A proxy that inspects TLS is usually trusted
 * through the system store only.
 */
function trustedCertificates(): string[] {
  certificates ??= [
    ...new Set([
      ...tls.getCACertificates("default"),
      ...tls.getCACertificates("system"),
    ]),
  ];
  return certificates;
}

let agents: { http: http.Agent; https: https.Agent } | undefined;

/**
 * The agent for `url`. Both read `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY` (either case) from
 * the environment when they are made, on the first download in this process.
 */
function agentFor(url: URL): http.Agent {
  agents ??= {
    http: new http.Agent({ proxyEnv: process.env } as ProxyAgentOptions),
    https: new https.Agent({
      proxyEnv: process.env,
      ca: trustedCertificates(),
      keepAlive: false,
    } as ProxyAgentOptions),
  };
  return url.protocol === "https:" ? agents.https : agents.http;
}

/**
 * Requests `url` with GET and resolves with the response once its headers arrive, after following
 * up to five redirects (never from `https:` to `http:`). The caller checks the status and reads or
 * discards the body. Rejects with the request's error; `ETIMEDOUT` when no data arrives for
 * {@link HTTP_SETTINGS}`.idleTimeoutMs`, also while the body is being read.
 */
export async function get(
  url: string,
  signal?: AbortSignal,
): Promise<IncomingMessage> {
  let current = new URL(url);
  for (let hop = 0; ; hop += 1) {
    const response = await request(current, signal);
    const status = response.statusCode ?? 0;
    const location = response.headers.location;
    if (![301, 302, 303, 307, 308].includes(status) || location === undefined)
      return response;
    response.resume();
    if (hop === MAX_REDIRECTS)
      throw new Error(`more than ${MAX_REDIRECTS} redirects`);
    const next = new URL(location, current);
    if (current.protocol === "https:" && next.protocol !== "https:")
      throw new Error(`redirected to an insecure URL ${next.href}`);
    current = next;
  }
}

function request(url: URL, signal?: AbortSignal): Promise<IncomingMessage> {
  if (url.protocol !== "https:" && url.protocol !== "http:")
    return Promise.reject(new Error(`unsupported URL scheme ${url.protocol}`));
  const client = url.protocol === "https:" ? https : http;
  return new Promise((resolve, reject) => {
    const outgoing = client.get(url, {
      agent: agentFor(url),
      ...(signal ? { signal } : {}),
    });
    let incoming: IncomingMessage | undefined;
    outgoing.setTimeout(HTTP_SETTINGS.idleTimeoutMs, () => {
      const seconds = HTTP_SETTINGS.idleTimeoutMs / 1000;
      const error = Object.assign(new Error(`no data for ${seconds} s`), {
        code: "ETIMEDOUT",
      });
      // Before the headers this rejects the promise; after them it fails the body's reader.
      incoming?.destroy(error);
      outgoing.destroy(error);
    });
    outgoing.on("error", reject);
    outgoing.on("response", (response) => {
      incoming = response;
      resolve(response);
    });
  });
}
