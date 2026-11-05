# OpenSSF posture

This page records how transcode maps to the OpenSSF Scorecard checks and the Best Practices
badge, what is already in the repository, and what still needs a human. Status words are used
strictly: **In repo** means a file or setting exists in this repository. **Verified** means it was
checked by running it. **Human** means someone with access has to act. Nothing here is a claim
that a score has been earned.

## Scorecard checks

| Check | What closes it | Status |
|---|---|---|
| Pinned-Dependencies | Every `uses:` is a full commit SHA. Tags are verified with `git ls-remote` (see docs/SUPPLY-CHAIN.md). `cargo` dependencies are pinned by `Cargo.lock`. | In repo; Verified for the SHAs listed in SUPPLY-CHAIN.md |
| SAST | `codeql.yml` (Rust, `security-extended`) and `cargo clippy -D warnings` in CI | In repo |
| Fuzzing | `fuzz/` with seven libFuzzer targets (`libfuzzer_sys` marker) and `fuzz.yml` (pull requests and daily) | In repo; targets compile on stable (`cargo check`). No long campaign run yet |
| Signed-Releases | `release.yml`: cosign keyless signing bundles (`.sigstore.json`), SLSA provenance (`.intoto.jsonl`) | In repo; not yet exercised by a real tag |
| Packaging | Not applicable yet. The crate sets no `publish` flag. Release assets are GitHub release tarballs, not a package registry | Human (decide whether to publish to crates.io) |
| Branch-Protection | Ruleset (see below) | Human. Not applied |
| CI-Tests | `ci.yml` runs build, tests (with ffmpeg), fmt and clippy on every pull request | In repo |
| Dependency-Update-Tool | `dependabot.yml` for cargo and github-actions, weekly | In repo |
| Token-Permissions | Read-only top-level `permissions` in every workflow; write only per job | In repo |
| Security-Policy | `SECURITY.md` | In repo |
| License | `LICENSE` (Apache-2.0) matches `license = "Apache-2.0"` in Cargo.toml | In repo |
| Binary-Artifacts | No binaries are committed. The crate has no build scripts. | Verified by inspection |
| Dangerous-Workflow | No `pull_request_target`; untrusted input is not interpolated into `run:` steps | In repo |
| Vulnerabilities | `cargo audit` and `cargo deny` in `supply-chain.yml`. Last local run: no advisories | In repo; Verified locally |
| Maintained | Time-gated. The project must have commits over 90 days | Time |
| Code-Review | Requires human approvals on each change through the ruleset | Human |
| Contributors | Requires contributors from more than one organisation. Not claimed | Human |
| CII-Best-Practices | See the next section | Human |
| Webhooks | Not applicable: no webhook-driven service | Not applicable |

## Best Practices badge (bestpractices.dev)

Registration needs an OAuth login, which cannot be done from this repository. No badge ID has
been created or recorded here.

| Area | Status |
|---|---|
| Basics: FLOSS license, documented project, public repository | License in repo. The repository is **private** today; it must be public before the badge is meaningful. **Human** |
| Change control: version control, unique version per release | In repo (git, Cargo version, tags via release workflow) |
| Reporting: bug reports and vulnerability reports | `SECURITY.md`, issue templates. Response targets are proposed, not yet agreed. **Human** |
| Quality: build, automated tests, warnings policy | CI and `-D warnings` for correctness and suspicious lints |
| Security: secure design, no unsafe, dependency checks | `#![forbid(unsafe_code)]`; cargo-audit and cargo-deny in CI. Fuzzing exists but has not run for a sustained period |
| Analysis: static analysis | Clippy and CodeQL in CI |

## Open items

1. **CODEOWNERS.** Resolved. `.github/CODEOWNERS` lists `@twn-lloyd @yokoszn`, two human owners,
   as the owners instructed. The ruleset still needs applying (item 6). No bot accounts are listed.
2. **Patents and licensing.** Resolved. The code is Apache-2.0, all modules are published, and the README and docs/STANDARDS.md state the patent position of each format and that the licence grants no rights under third-party patents.
3. **Contacts.** Resolved. SECURITY.md lists security@twn.systems and CODE_OF_CONDUCT.md lists
   hello@twn.systems, alongside GitHub private reporting.
5. **Visibility.** The repository is private. Scorecard publishing, the badges and
   dependency-review on private repositories (which needs GitHub Advanced Security) depend on this.
   **Human**
6. **Ruleset.** Default-branch protection is not applied. The caretaker ruleset requires two
   approvals, code-owner review and the `Rust checks` status check. Apply it only after the
   CODEOWNERS placeholders are resolved. **Human**
7. **Fuzz campaigns.** Targets compile. A sustained campaign has not been run. The first CI
   fuzz run may find real panics; those are bugs to fix, not to suppress. **Human review of
   results**

## Caretaker

`caretaker-ossf` holds the fleet template set this repository follows
(`ptyktos/caretaker`, `caretaker-ossf/templates/`). The `caretakerctl ossf` commands are not run
for this repository yet:

```sh
# Render the template set offline and inspect the result.
caretakerctl ossf render --repo-path webfold-ai/transcode --crate-name transcode \
  --import-entry transcode::png::decode --code-owners '@owner-one @owner-two' --out /tmp/transcode-ossf

# Measure compliance (read-only). Needs the GitHub token in CARETAKER_GITHUB_TOKEN.
caretakerctl ossf report --repos webfold-ai/transcode

# Open the compliance change. Use --skip-ruleset until the owners are real and the change is merged.
caretakerctl ossf apply --repo webfold-ai/transcode --crate-name transcode \
  --import-entry transcode::png::decode --code-owners '@owner-one @owner-two' --skip-ruleset
```

These are the commands documented in `caretaker/docs/ossf-scorecard.md`. This repository
diverges from the template set on purpose: its fuzz targets are per decoder (not the HTTP
service pair), it has no container job, and its release job builds the `transcode` CLI. Do not let
`apply` overwrite those files without review. The caretakerctl binary could not be built in the
session that produced this change, because the caretaker workspace depends on private git
repositories. The template files were rendered with a Python script that follows the same
substitutions and invariants.
