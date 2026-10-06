<p align="center">
  <img src="assets/app-icon.png" width="128" alt="Clean You icon">
</p>

<h1 align="center">Clean You</h1>

<p align="center">A fast, free, open-source Mac cleaner written in Rust.</p>

<p align="center">
  <a href="https://github.com/luckyklyist/cleanupmacbro/releases/latest/download/CleanYou.dmg"><img alt="Download for macOS" src="https://img.shields.io/github/v/release/luckyklyist/cleanupmacbro?label=Download%20for%20macOS&style=for-the-badge&logo=apple&logoColor=white&color=0a84ff"></a>
</p>

<p align="center">
  <a href="https://github.com/luckyklyist/cleanupmacbro/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/luckyklyist/cleanupmacbro?color=0a84ff"></a>
  <img alt="macOS 11+" src="https://img.shields.io/badge/macOS-11%2B-black">
  <img alt="Universal" src="https://img.shields.io/badge/Apple%20Silicon%20%2B%20Intel-555">
  <a href="LICENSE"><img alt="MIT" src="https://img.shields.io/badge/license-MIT-green"></a>
</p>

## Features

- **Smart Clean**: caches, logs and rebuildable dev folders in one click
- **Find space**: large files, duplicates, old installers, local AI models, games
- **Disk Map**: sunburst view of any drive or folder, with snapshots
- **Uninstaller**: removes apps together with their leftovers
- **Privacy, Startup Items, Performance**: browser data, login items, CPU and RAM hogs
- **Battery Care**: 80% charge limit, heat guard, menu bar and smart alerts

Everything goes to the Trash first. No telemetry, no account.

## Install

1. Download **[CleanYou.dmg](https://github.com/luckyklyist/cleanupmacbro/releases/latest/download/CleanYou.dmg)** (latest version).
2. Open it and drag **Clean You** onto **Applications**.
3. First launch: right-click the app → **Open** (it isn't notarized), or run:

```bash
xattr -dr com.apple.quarantine "/Applications/Clean You.app"
```

A short welcome tour sets up Full Disk Access and runs your first scan.
To update later, press **Check for updates** at the bottom of the sidebar.

## Build from source

Needs [Rust](https://rustup.rs) and the Xcode Command Line Tools.

```bash
UNIVERSAL=1 ./build_app.sh && ./make_dmg.sh
```

This writes `Clean You.app` and `dist/CleanYou-<version>.dmg`. Pushing a `v*` tag builds and publishes a release.

## License

[MIT](LICENSE). Contributions welcome, see [CONTRIBUTING.md](CONTRIBUTING.md).
