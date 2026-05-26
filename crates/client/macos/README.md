# macOS packaging

This folder holds the macOS-specific artifacts cargo-bundle reads when
producing `Toki.app`. The bundling config itself lives in
`crates/client/Cargo.toml` under `[package.metadata.bundle]`.

## One-time setup

```bash
cargo install cargo-bundle
```

`cargo-bundle` is a third-party cargo subcommand (not part of the
toolchain). Install it once per dev machine.

## Producing the app

```bash
# from the repo root
cargo bundle --release -p toki-client
```

Output:

```
target/release/bundle/osx/Toki.app
```

You can drag that into `/Applications`, double-click to launch, or
zip it for distribution.

For a debug build (faster turnaround, larger binary):

```bash
cargo bundle -p toki-client
# → target/debug/bundle/osx/Toki.app
```

## What ends up inside the bundle

```
Toki.app/
├── Contents/
│   ├── Info.plist           ← merged from cargo-bundle defaults + Info.plist.ext.xml
│   ├── MacOS/
│   │   └── toki             ← the binary (named per [[bin]].name)
│   └── Resources/
│       └── Toki.icns        ← from crates/client/assets/icon/Toki.icns
```

Key Info.plist entries (a few are worth knowing about):

| Key | Value | Why |
|---|---|---|
| `CFBundleIdentifier` | `com.github.c-t-n.toki` | Reverse-DNS, must be unique system-wide. |
| `CFBundleVersion` | from workspace `version` | Cargo handles this. |
| `NSMicrophoneUsageDescription` | "Toki uses the microphone…" | Required for audio capture on 10.14+. |
| `NSAppTransportSecurity / NSAllowsArbitraryLoads` | `true` | Allows the default `http://` gRPC URL to reach the server. |
| `LSMinimumSystemVersion` | `10.15` | cpal + eframe floor. |
| `NSHighResolutionCapable` | `true` | Retina rendering. |

## Code signing

Unsigned bundles run fine on the dev machine that built them but
trigger Gatekeeper warnings on other Macs. For local distribution:

```bash
# Ad-hoc signature — silences the unidentified-developer warning on
# the same Mac but not on others.
codesign --force --deep --sign - target/release/bundle/osx/Toki.app
```

For real distribution you need a Developer ID Application certificate
and a notarisation pass:

```bash
codesign --force --deep --options runtime \
    --sign "Developer ID Application: Your Name (TEAMID)" \
    target/release/bundle/osx/Toki.app

# Then notarise — xcrun notarytool submit ... wait, staple, etc.
```

Out of scope for the codebase itself; documented here for completeness.

## Updating the icon

Toki.app's icon is `Toki.icns` from `crates/client/assets/icon/`. If
you change any of the source PNGs there, rerun:

```bash
crates/client/assets/icon/build-platform-icons.sh
```

…then `cargo bundle --release -p toki-client` again to repackage.
