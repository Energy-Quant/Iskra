# CI and package matrix

The authoritative pipeline is .github/workflows/ci.yml, with reusable Linux
checks in linux.yml and privileged delivery in delivery.yml.

- Linux lanes run in parallel: Rust 1.96.0 locked tests/fmt/clippy, native
  packaging, Lua/i18n/installer/packaging/Ed25519 script checks, and a standalone
  SHA256-verified RustSec audit. Luau source builds and actionlint are pinned;
  runtime applications are never launched.
- An exact fast-forward synchronization push that already matches main skips
  duplicate dev build lanes; the branch-policy gate still runs.
- Windows/macOS/Linux: Python 3.12 and Pillow 13.0.0, avatar/portable tests,
  unsigned plugin staging ZIPs and avatar authoring toolkits.
- CI Gate: every required dependency must succeed. A skipped, cancelled or
  failed build does not qualify for a stable release or promotion.

Current native desktop package: Linux/Wayland x86_64. Windows/macOS ZIPs
contain a portable plugin/toolkit; they are not desktop executables.

Stable/nightly delivery, source-SHA checks, signature review, App provisioning
and active server rules are documented in DEVELOPMENT.md. Zeron private keys
are never placed in CI. The release App key authorizes GitHub operations only.
