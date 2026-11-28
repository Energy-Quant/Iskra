# Installation and platform support

## Zeron plugin — ordinary installation

1. Install [Zeron](https://zeron.sh).
2. Open Settings → Plugins → Install and use
   https://github.com/Energy-Quant/Iskra/releases/latest, or select its signed
   `.zeron-plugin` asset. Review who signed it and the permission request.
3. Allow it in Zeron. Users do not claim ownership or sign someone else's plugin.

The raw signed plugin is platform-independent. The new delivery pipeline packages that
same plugin with instructions in OS-labelled ZIPs; the existing v0.1.0
release provides the raw portable plugin only. They do not contain a Windows/macOS native
companion executable. Packaging runs on Windows, macOS and Linux in CI.

If you developed Iskra locally, export and preserve your work first. Remove
that development copy through Zeron Settings before installing the release.
A developer-folder install is not the public update channel.

## Optional desktop — Linux x86_64 and Windows x86_64

Download the desktop tarball and SHA256SUMS from the stable release. Verify
the tarball checksum, extract it and run its install.sh. This installs
`~/.local/bin/zeron-pet` with versioned assets. Requires GTK4,
gtk4-layer-shell, Wayland and a supported compositor (Hyprland/wlroots).
No application or autostart is started by the installer.

Launch the installed executable. Do not launch an old checkout's run.sh:
it can carry old assets. Updates use the next release's archive/installer.
The optional panel bridge is documented in ASHELL.md.

### Windows (source build)

The Win32 port is a portable `zeron-pet.exe`: a topmost layered window
updated via `UpdateLayeredWindow`, plus a matching HUD window; single
instance via a session mutex + named pipe. It builds without MSVC or a
mingw toolchain through the `x86_64-pc-windows-gnullvm` Rust target —
repo `.cargo/config.toml` already pins `rust-lld` + `crt-static`:

```powershell
rustup toolchain install 1.96.0-x86_64-pc-windows-gnullvm
cargo +1.96.0-x86_64-pc-windows-gnullvm build --release \
  --target x86_64-pc-windows-gnullvm
```

`--monitor` takes the Windows device name (`DISPLAY2`), shown by
`EnumDisplayMonitors` in `winmon`. Fullscreen detection only hides the pet
on the monitor hosting the fullscreen window; maximized windows do not
count. Control flags (`--status`, `--pause`, `--click-through on|off`,
`--quit`) forward over the named pipe to the running instance, same as on
Wayland.

### Devin CLI status bridge (Windows)

`zeron-pet-devin-mcp` is a second small binary that feeds the pet the
live status of local Devin CLI/Desktop sessions instead of a Zeron
transport. Build it with the same toolchain, then place both executables
and the `assets/` directory in one folder. One-time setup:

```powershell
cargo +1.96.0-x86_64-pc-windows-gnullvm build --release --bin zeron-pet-devin-mcp
# copy zeron-pet.exe, zeron-pet-devin-mcp.exe and assets\ into one folder, then:
.\zeron-pet-devin-mcp.exe install
```

`install` merges lifecycle hook entries into the Devin config
(`%APPDATA%\devin\config.json`) without touching other keys and is
idempotent. From then on every session start launches the pet (single
instance — parallel sessions share it), and the pet mirrors session
state: working while the agent acts, a question pose on permission and
input prompts, joy when a turn completes. Uninstall by removing the
`zeron-pet-devin-mcp` hook entries from the config.

## Nightly channel

`nightly-<commit>-<run>` releases are CI-only, immutable prereleases from
successful dev CI. Their plugin ZIPs are UNSIGNED developer staging, not
installable signed plugins. Avatar tools and Linux native binaries are
previews. Stable publication cannot reuse a nightly signature or skip gates.

## Building and signing

Build instructions and the headless/live boundary are in PLUGIN.md and CI.md.
Signing and export happen on the author's device, with Zeron's consent.
CI checks the exact files and Ed25519 signature but never signs the plugin.
