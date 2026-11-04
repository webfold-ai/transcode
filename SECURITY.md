# Security policy

## Supported versions

| Version | Supported |
|---|---|
| 0.1.x (current development line) | Yes |
| Anything older | No |

transcode has not had a public release yet. Until one exists, only the default branch is
supported.

## Reporting a vulnerability

Please report security issues privately, using GitHub's private vulnerability reporting:

**https://github.com/webfold-ai/transcode/security/advisories/new**

Do not open a public issue, pull request or discussion for a suspected vulnerability.

Include, where you can:

- the affected version or commit,
- a minimal input (a byte file, or a generator script) that reproduces the problem,
- the observed behaviour (panic message, hang, memory growth, wrong output) and the expected behaviour.

## Scope

In scope:

- Panics, out-of-bounds access, unbounded allocation, or non-terminating loops in any decoder
  or parser that accepts untrusted bytes (PNG, JPEG, FLAC, SLAC, MP3, AAC, Opus demux/parse,
  and the container readers that are used on input).
- Incorrect output that could be used to mislead a consumer, for example a decoder that accepts
  a malformed stream as valid and returns different content from the reference decoder.
- Problems in the release process or in the supply-chain configuration described in
  [docs/SUPPLY-CHAIN.md](docs/SUPPLY-CHAIN.md).

Out of scope:

- Speed and output-quality issues that do not affect safety.
- Missing features listed in [ROADMAP.md](ROADMAP.md) (for example, Opus packet-to-PCM decode).
- Patent or licensing questions. These are not security issues; see the README.

## What to expect

We aim to acknowledge a report within 7 days and to give a first assessment within 14 days.
This is a small project, so those targets are best effort. We will keep you informed, credit you
in the advisory if you want, and coordinate disclosure with you.
