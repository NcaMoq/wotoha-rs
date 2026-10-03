# Security policy

## Supported versions

The `main` branch and the latest digest-pinned production image are the
supported security targets. Operators should use a full digest image reference and keep
the host, Docker Engine, and host credentials updated.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting for this repository when
available. If it is unavailable, open an issue containing only a brief,
non-sensitive description and request a private channel; do not publish
credentials, exploit code, raw user data, or private media.

Include the affected commit or image tag, impact, reproduction conditions,
and a proposed mitigation when safe to share. We will acknowledge a report
when practical and coordinate disclosure after a fix or mitigation is
available.

## Runtime security boundary

The supported container runs as a non-root user with a read-only root
filesystem, no added Linux capabilities, no-new-privileges, bounded temporary
storage, bounded queues, bounded provider response bodies, and rotated logs.
Do not run it with `--privileged`, extra capabilities, or disabled security
profiles as a workaround.
