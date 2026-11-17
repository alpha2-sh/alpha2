# Security policy

## Reporting a vulnerability

Report it privately through GitHub's private vulnerability reporting: the repository's Security tab, then "Report a vulnerability" (<https://github.com/alpha2-sh/alpha2/security/advisories/new>). Do not open a public issue for it.

## Supported versions

alpha2 is before 1.0: fixes go into the next release, and only the latest release is supported.

## What counts

- Memory unsafety in alpha2's own code or in how it calls a library.
- A boundary crossed: a plugin's sandbox escaped; a file read or written, or a network address reached, beyond what the request or the operator's flags name; a request seeing another request's data.
- A crafted media file, project, compose spec, font, document, caption file, effect or plugin that crashes or hangs a surface past the limits [docs/security.md](docs/security.md) states.

Not a vulnerability: resource use by a caller the deployment trusts, and anything [docs/security.md](docs/security.md) lists as unbounded or as the operator's to contain.
