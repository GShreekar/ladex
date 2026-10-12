# Security Policy

## Reporting a vulnerability

Please report security problems privately, through GitHub's [private vulnerability reporting](https://github.com/GShreekar/ladex/security/advisories/new) for this repository. Don't open a public issue, pull request or discussion for them.

Include what you found, how to reproduce it, and what an attacker could do with it. You should get a reply within a week. Once a fix is released, the advisory is published, crediting you unless you'd rather not be named.

## Supported versions

Only the latest release gets security fixes.

## What is in scope

LADEX is meant for trusted local networks. These are in scope:

- Someone on the same network who can watch or change traffic (ARP or DNS spoofing, a rogue node).
- A node or browser that knows the passphrase but sends hostile data: path traversal, oversized or malformed messages, zip bombs.
- Guessing the passphrase, online or offline.
- A lost or stolen device whose key should no longer be trusted.
- A web page in the user's browser trying to reach the node (cross-site requests, DNS rebinding).

These are out of scope:

- A device whose operating system is already compromised.
- Someone with physical access to an unlocked device.
- Traffic analysis: who talks to whom, and how much.
- Nodes started without a passphrase. They accept anyone, and say so in a warning on every page.

## How LADEX protects itself

See *Security Model* in the [README](README.md) for a summary.
