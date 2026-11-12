
### Install

```sh
curl -fsSL https://raw.githubusercontent.com/ts-ign0re/makedo/main/install.sh | sh
```

The script picks the archive for your platform, verifies its SHA-256 checksum and puts `makedo` in `~/.local/bin`. Set `MAKEDO_INSTALL_DIR` to use another directory. Then run `makedo` in a project folder; on the first run it asks you to pick a provider and a model.

On macOS, add `--app` to install the app as well. It goes to `/Applications`, or to `~/Applications` if that folder is not writable:

```sh
curl -fsSL https://raw.githubusercontent.com/ts-ign0re/makedo/main/install.sh | sh -s -- --app
```

To install exactly this version rather than the latest one:

```sh
curl -fsSL https://raw.githubusercontent.com/ts-ign0re/makedo/main/install.sh \
  | MAKEDO_RELEASE_URL=https://github.com/ts-ign0re/makedo/releases/download/{{TAG}} sh
```

### Downloads

| Platform | Archive |
|---|---|
| Linux x86_64, any distribution (static) | `makedo-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64, any distribution (static) | `makedo-aarch64-unknown-linux-musl.tar.gz` |
| Linux x86_64, glibc 2.35 or newer | `makedo-x86_64-unknown-linux-gnu.tar.gz` |
| macOS, Apple Silicon | `makedo-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `makedo-x86_64-apple-darwin.tar.gz` |
| macOS app, Apple Silicon | `Makedo-aarch64-apple-darwin.app.tar.gz` |
| macOS app, Intel | `Makedo-x86_64-apple-darwin.app.tar.gz` |

Every archive has a `.sha256` file next to it. To check one by hand, download both and run `shasum -a 256 -c makedo-<platform>.tar.gz.sha256`.

The macOS binaries and the app are ad-hoc signed. If you download one through a browser, macOS quarantines it; clear the flag with `xattr -dr com.apple.quarantine <path>`. The install script does not need this.

Windows is not supported.

### Requirements

- A model: an API key, a ChatGPT subscription or local Ollama.
- Git, for the wiki, memory and worktrees; `makedo finish` needs 2.38 or newer.

Licensed under GPL-3.0-or-later.
