-- Throwaway Prosody for Piopio's tests. Plaintext on 127.0.0.1 only;
-- never use this configuration for a real server.
-- run.sh fills in @DIR@ and @USER@; newer Prosody versions do not let the
-- configuration read environment variables.
local dir = "@DIR@"

daemonize = false
-- CI runs as an ordinary user; containers often run as root.
run_as_root = true
prosody_user = "@USER@"
pidfile = dir .. "/prosody.pid"
data_path = dir .. "/data"
certificates = dir .. "/certs"
log = { debug = dir .. "/prosody.log" }

admins = {}
modules_enabled = {
    "roster", "saslauth", "disco", "carbons", "pep", "private", "blocklist",
    "vcard4", "vcard_legacy", "version", "uptime", "time", "ping", "register",
    "mam", "smacks", "csi_simple", "offline", "bookmarks",
}
modules_disabled = { "s2s", "tls", "posix" }

-- File uploads (XEP-0363) over plain HTTP on 127.0.0.1.
http_ports = { 15280 }
http_interfaces = { "127.0.0.1" }
https_ports = {}
http_default_host = "upload.localhost"

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
    disco_items = { { "upload.localhost" }, { "conference.localhost" } }

Component "conference.localhost" "muc"
    modules_enabled = { "muc_mam" }
    restrict_room_creation = false
    muc_room_default_persistent = false
    muc_room_default_public = true

Component "upload.localhost" "http_file_share"
    http_file_share_size_limit = 10 * 1024 * 1024
    http_external_url = "http://127.0.0.1:15280/"
