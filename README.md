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

**Status:** early development. See [What works](#what-works).

## What works

- **Accounts:** add, log in (direct TLS where the server publishes it, else
  STARTTLS; SCRAM), turn off and on, change the password, remove, set your
  picture. Accounts and everything else live in an encrypted SQLCipher
  database whose key is kept in the keyring (the Secret portal inside
  Flatpak), and reconnect when the daemon starts. The server is told
  whether a window is open (Client State Indication), so it can hold back
  unurgent traffic while none is.
- **Contacts:** roster with versioning, presence, adding and renaming
  contacts, accepting or declining contact requests, blocking (XEP-0191,
  shared with your other devices; a blocked contact leaves the list and is
  unblocked from Preferences).
- **One-to-one chat:** sending and receiving, delivery receipts and read
  markers, typing notifications, Carbons for your other devices, and
  catch-up from the server archive (MAM) after being offline, with fuller
  history and the whole roster on first setup; duplicates are dropped.
- **OMEMO**, both versions, implemented in pure Rust in `piopio-omemo` and
  checked in CI against python-oldmemo and python-twomemo in both
  directions. The original version (`eu.siacs.conversations.axolotl`, what
  Conversations, Dino, Gajim, Monal and Converse.js speak) is on by default
  whenever the other side has it. OMEMO 2 (`urn:xmpp:omemo:2`, with
  stanza content encryption) is used, in one-to-one and group chats, when
  every device involved reads it, and never mixed with the original in
  one chat. Blind Trust Before Verification, and verifying devices by
  comparing fingerprints: a contact's in Conversation Details, your own
  from each account's Encryption page in Accounts. Messages are never
  sent in the clear to someone who uses OMEMO.
- **Replies, corrections, reactions (any emoji), deleting for everyone**
  (XEP-0461, 0308, 0444, 0424), from each message's menu.
- **Group chats:** joining (Ctrl+J), history from the room's archive,
  occupants, bookmarks shared with your other devices, leaving and
  rejoining, invitations. Private rooms (members-only, non-anonymous) can
  be created from the Join dialog and are OMEMO encrypted.
- **Files:** sharing through the server's upload service, encrypted
  (`aesgcm://`) in OMEMO chats; without an upload service, straight to the
  contact's device over Jingle (XEP-0234, in-band bytestreams). Pasting a
  picture offers to send it. Pictures download by themselves and get
  thumbnails made by the daemon; animated GIF, APNG and WebP play.
- **Calls:** voice and video calls with anyone whose client speaks Jingle
  RTP (XEP-0167) with ICE and DTLS-SRTP, rung through Jingle Message
  Initiation (XEP-0353) so every device rings. The daemon does the
  signalling; the media (Opus, VP8) runs in the window through GStreamer's
  webrtcbin, in `piopio-call`.
- **Background and notifications:** the daemon runs with no window, can
  start at login (the Background portal inside Flatpak), and shows
  notifications while no window is open (in group chats only when you're
  mentioned), withdrawn once read anywhere. A conversation can override
  Do Not Disturb, including feedbackd's Quiet and Silent modes on phones.
- **Window:** a conversation list after Dino's (pictures, times that follow
  the desktop's 12/24-hour setting, pinned and muted marks, typing,
  previews), messages grouped by sender with delivery and read ticks, a
  padlock wherever a message isn't encrypted, a message box that grows
  with its Send button attached, Conversation Details (contact, display
  name, encryption, pinning, muting, typing and read receipts, Block,
  Delete), Preferences, and a Quit that really quits; adapts down
  to 360px. Features a contact's app doesn't support aren't offered.

Not yet:

- **Group calls.** Dino and Movim make them with Muji (XEP-0272) and Call
  Invites (XEP-0482): every participant calls every other through the
  room. It needs a call window for several streams and testing against
  both; one-to-one calls work with everyone.
- **SASL2, Bind 2 and FAST** (faster, password-free reconnects). tokio-xmpp
  6 can neither read SASL2's elements nor accept a stream bound at login,
  so these wait for the library.
- **Encrypted Jingle file transfer** (JET): no two clients agree on it yet,
  so direct files only go in unencrypted conversations; encrypted ones use
  the upload service.

## Layout

| Path | What it is |
| --- | --- |
| `crates/piopio-daemon` | The background service (`net.dunevent.Piopio.Daemon`) |
| `crates/piopio` | The UI (`net.dunevent.Piopio`) |
| `crates/piopio-dbus` | Bus names, object paths and client proxies shared by both |
| `crates/piopio-omemo` | OMEMO and OMEMO 2 (X3DH, Double Ratchet, XEdDSA) in pure Rust, sans I/O |
| `crates/piopio-call` | Call media: GStreamer webrtcbin, run by the UI |
| `tests/server` | A throwaway local Prosody for the end-to-end tests |
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
cargo test -p piopio-daemon -p piopio-dbus  # needs dbus-daemon

# End-to-end tests against a throwaway local Prosody (needs prosody installed):
tests/server/run.sh cargo test -p piopio-daemon
```

The login test against a real server is ignored unless asked for:

```sh
PIOPIO_TEST_JID=... PIOPIO_TEST_PASSWORD=... \
  cargo test -p piopio-daemon --test server -- --ignored
```

Set `PIOPIO_LOG=debug` for more daemon output.

## Name

The piopio was a New Zealand thrush, now extinct. The North Island piopio was
named in te reo Māori after its call, a short, sharp whistle quickly repeated.

## Licence

GPL-3.0-or-later. See [COPYING](COPYING).
