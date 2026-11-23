# Piopio

An XMPP client for GNOME, built in two parts:

- **`piopio-daemon`** does all the XMPP work: connections, storage, OMEMO,
  notifications. It never loads GTK, and it is built to run in the background
  with no window open while using little memory.
- **`piopio`** is the GTK 4 and libadwaita window. It only shows what the
  daemon knows and sends the user's actions to it over D-Bus. Closing it ends
  the process and frees its memory.

Goals: users of Conversations, Dino and Converse.js should never notice a
difference, and OMEMO is on from the first message.

**Status:** early development. Nothing works yet beyond the daemon claiming its
bus name and the window opening.

## Layout

| Path | What it is |
| --- | --- |
| `crates/piopio-daemon` | The background service (`net.dunevent.Piopio.Daemon`) |
| `crates/piopio` | The UI (`net.dunevent.Piopio`) |
| `crates/piopio-dbus` | Bus names, object paths and client proxies shared by both |
| `data/` | Desktop file, metainfo, D-Bus service file, icons |
| `build-aux/` | Flatpak manifest for the GNOME 51 runtime |
| `piopio.doap` | Description for xmpp.org's software list |

## Building

The supported build is the Flatpak on the GNOME 51 runtime:

```sh
flatpak-builder --user --install --force-clean build-dir build-aux/net.dunevent.Piopio.Devel.yml
flatpak run net.dunevent.Piopio
```

For quick work outside Flatpak:

```sh
cargo build -p piopio-daemon
cargo run -p piopio --no-default-features   # GTK 4.12+ and libadwaita 1.4+
dbus-run-session -- cargo test -p piopio-daemon -p piopio-dbus
```

Set `PIOPIO_LOG=debug` for more daemon output.

## Name

The piopio was a New Zealand thrush, now extinct. The North Island piopio was
named in te reo Māori after its call, a short, sharp whistle quickly repeated.

## Licence

GPL-3.0-or-later. See [COPYING](COPYING).
