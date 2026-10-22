# Security policy

## Supported versions

Only the latest released version of vacancyd receives security fixes. Development builds and older releases are unsupported.

## Reporting a vulnerability

Do not open a public issue. Report privately, either through GitHub private vulnerability reporting under **Security > Report a vulnerability**, or by email to [security@probablyfine.dev](mailto:security@probablyfine.dev).

Include the affected version or commit, operating system, impact, required configuration, and the smallest reproduction you can provide. Redact credentials, bearer tokens, private hostnames, and network details.

Please allow time to reproduce and fix a confirmed issue before publishing details.

## Security boundary

vacancyd is designed for a trusted LAN. It does not provide TLS, so bearer tokens travel over HTTP unless TLS is terminated in front of it. A TLS proxy, port forward or tunnel on the same machine delivers its clients from loopback, so it requires `trust_loopback = false` and tokens for every client. Remote listeners require configured authentication unless the operator enables the explicit unauthenticated-remote escape hatch.

Local processes are not all the owner. On Windows and Linux, a loopback connection without a token may read status and settings and pause lending, and nothing else. Changes that can increase lending or rewrite the protected configuration need an administrator the service identifies from the kernel, or an `admin` token: on Windows an elevated administrator or LocalSystem, from the caller's token over the admin pipe; on Linux root or a member of the `vacancyd-admin` group, from the peer credentials of the admin socket `/run/vacancyd/admin.sock`. On macOS loopback trust still grants every admin action in this release; set `trust_loopback = false` and use tokens on a machine with untrusted local users. Upstream inference services should remain bound to loopback so vacancyd is the only borrower-facing route.

See [docs/authentication.md](docs/authentication.md) for token scopes and deployment assumptions.
