# Security

## The protocol is not a security boundary

Said first, because it is the thing most likely to matter to you:

**micbridge has no authentication and no encryption.** Media travels as plain PCM
in UDP datagrams, and the control channel is unencrypted TCP. Anyone who can reach
the receiver's port can send audio to it, and anyone who can observe the network
can listen to what is being sent.

The receiver drops datagrams whose source address does not match the peer that
completed the handshake. That is hygiene against stray traffic, not a security
control — a source address is trivially forged on a network where an attacker can
already send packets.

Treat the channel as public and run it accordingly: a wired LAN you control, or an
encrypted overlay such as [Tailscale](https://tailscale.com) or WireGuard, which is
how the author runs it. `docs/protocol.md` describes what an authenticated version
would need.

Also worth stating plainly: the payload is an open microphone. Everything the
sending machine's input hears goes onto the network for as long as a session runs.

## Reporting a vulnerability

Open a [security advisory](https://github.com/dieterpl/micbridge/security/advisories/new),
or a normal issue if it is not sensitive. This is a hobby project maintained by one
person — expect a considered reply rather than a fast one, and no bounty.

Please do report:

- A way to make the receiver execute or crash on malformed input. The framing and
  media parsers are the interesting surface, and both are reachable pre-handshake.
- Anything that escapes the stated model above — for example a way to read memory
  or files rather than merely send audio.

Please do not report that the protocol is unauthenticated or unencrypted. That is
documented, deliberate for the intended deployment, and above.

## Supported versions

The latest release only.
