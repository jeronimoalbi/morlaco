# Morlaco

[![Test](https://github.com/jeronimoalbi/morlaco/actions/workflows/test.yml/badge.svg?branch=main)](https://github.com/jeronimoalbi/morlaco/actions/workflows/test.yml)
[![Audit](https://github.com/jeronimoalbi/morlaco/actions/workflows/audit.yml/badge.svg?branch=main)](https://github.com/jeronimoalbi/morlaco/actions/workflows/audit.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Paste in a Bitcoin address and see what's in it, what it's worth, and whether
you're up or down.

Morlaco is a small GNOME app. It reads the public blockchain, looks up what
Bitcoin cost on the days your coins arrived, and shows your estimated gain or
loss. Your saved data is encrypted on your computer.

## What you get

- Your balance and what it's worth in US dollars.
- Your gain or loss, for each wallet and for all of them together.
- Your coins, listed by the day they arrived and the price back then.
- A button that hides every number, for when someone is looking over your
  shoulder.
- Moves between your own wallets aren't treated as selling, so they don't mess
  up your numbers.

## Install

**Arch Linux:**

```sh
cd contrib/arch/local && makepkg -si
```

That builds the latest committed version and installs it with pacman. To remove
it later: `sudo pacman -R morlaco-git`.

**From source** (Rust 1.92 or newer, GTK 4, libadwaita 1.5 or newer, libsecret,
OpenSSL, a C compiler and pkgconf; on Arch,
`sudo pacman -S base-devel rust gtk4 libadwaita libsecret openssl`):

```sh
cargo build --release
sudo install -Dm755 target/release/morlaco /usr/bin/morlaco
sudo install -Dm644 contrib/desktop/io.github.jeronimoalbi.Morlaco.desktop /usr/share/applications/io.github.jeronimoalbi.Morlaco.desktop
sudo install -Dm644 app/data/icons/hicolor/scalable/apps/io.github.jeronimoalbi.Morlaco.svg /usr/share/icons/hicolor/scalable/apps/io.github.jeronimoalbi.Morlaco.svg
sudo install -Dm644 app/data/icons/hicolor/symbolic/apps/io.github.jeronimoalbi.Morlaco-symbolic.svg /usr/share/icons/hicolor/symbolic/apps/io.github.jeronimoalbi.Morlaco-symbolic.svg
```

Just want to try it? `cargo run` works from the checkout, no install needed.

## Using it

1. Open Morlaco and add a wallet: press **Ctrl+N**, paste an address, and give
   it a name if you like.
2. Give it a moment to read the blockchain. The first time takes the longest.
   After that, Morlaco shows what it saved right away and updates in the
   background.
3. Click **Overview** in the sidebar for the total across all your wallets, or
   click a wallet to see its details.

Handy keys:

| Key | What it does |
| --- | --- |
| Ctrl+N | Add a wallet |
| Ctrl+R or F5 | Refresh |
| Ctrl+H | Hide or show amounts |
| F9 | Hide or show the sidebar |
| Ctrl+Q | Quit |

Use the pencil and trash buttons next to a wallet to rename or remove it.

## Your data and privacy

- Everything Morlaco saves (your wallets, their history, prices) lives in one
  file, `~/.local/share/morlaco/morlaco.db`, and it's encrypted.
- The key is made for you and kept in your system keyring (GNOME Keyring, or
  any other Secret Service). That means Morlaco needs a keyring to open your
  data.
- Want a second lock? Open the main menu and choose **Passphrase…**. Morlaco
  will then ask for it every time it starts. If you forget it, your saved data
  can't be recovered.
- **Clear Local Data…** in the same menu deletes the file and the key.
- To read the blockchain and the price of Bitcoin, Morlaco asks
  [mempool.space](https://mempool.space). So that site sees which addresses
  you look up, along with the usual things any website sees, like your IP
  address.

## Good to know

- It's Bitcoin only for now. The code is ready for other coins, but they
  aren't there yet.
- The gain or loss is an estimate. A coin's "cost" is Bitcoin's price on the day
  it arrived at the address, which isn't always what you paid. If you bought on
  an exchange and withdrew later, those can differ.
- Payments that haven't been confirmed yet are left out until they are.
- These are rough numbers to help you understand your wallets, not tax or
  financial advice.

## Development

```sh
just check    # format, lints, tests and the dependency checks
cargo run     # run the app from the checkout
```

`just check` needs [`just`](https://github.com/casey/just) and
[`cargo-deny`](https://embarkstudios.github.io/cargo-deny/). The packaging
files are in `contrib/`.

## License

MIT. See `LICENSE`.
