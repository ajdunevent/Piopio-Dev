-- Throwaway Prosody for Piopio's tests. Plaintext on 127.0.0.1 only;
-- never use this configuration for a real server.
local dir = os.getenv("PIOPIO_TEST_SERVER_DIR")

daemonize = false
-- CI runs as an ordinary user; containers often run as root.
run_as_root = true
prosody_user = os.getenv("USER") or "root"
pidfile = dir .. "/prosody.pid"
data_path = dir .. "/data"
log = { debug = dir .. "/prosody.log" }

admins = {}
modules_enabled = {
    "roster", "saslauth", "disco", "carbons", "pep", "private", "blocklist",
    "vcard4", "vcard_legacy", "version", "uptime", "time", "ping", "register",
    "mam", "smacks", "csi_simple", "offline", "bookmarks",
}
modules_disabled = { "s2s", "tls", "http", "posix" }

c2s_ports = { 15222 }
c2s_interfaces = { "127.0.0.1" }
c2s_require_encryption = false
allow_unencrypted_plain_auth = true
authentication = "internal_hashed"
allow_registration = false

archive_expires_after = "never"
default_archive_policy = true
smacks_hibernation_time = 30

VirtualHost "localhost"
