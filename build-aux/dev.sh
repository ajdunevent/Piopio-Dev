#!/usr/bin/env bash
# Incremental development builds inside the Flatpak SDK.
#
# Prompts for the architecture(s) and what to produce, then writes .flatpak
# bundles to dist/ and/or installs the build for the current user. The crate
# cache and each architecture's compiled output persist in .flatpak/, so only
# changed code is recompiled. A Flatpak bundle holds a single architecture, so
# "Both" writes one file per architecture.
#
# Both programs, the window (piopio) and the background service
# (piopio-daemon), are built and installed together.
#
# Each Cargo profile keeps its own compiled output, so switching between them
# does not throw away the others.

set -euo pipefail

cd "$(dirname "$0")/.."

APP_ID=net.dunevent.Piopio
MANIFEST=build-aux/$APP_ID.Devel.yml
MODULE=piopio
RUNTIME_REPO=https://dl.flathub.org/repo/flathub.flatpakrepo
DIST=dist

host_arch=$(flatpak --default-arch)
other_arch=$([[ $host_arch == x86_64 ]] && echo aarch64 || echo x86_64)

choose() {
    local prompt=$1 default=$2
    shift 2
    local i=1
    for option in "$@"; do
        echo "  $i) $option" >&2
        i=$((i + 1))
    done
    local reply
    read -rp "$prompt [$default]: " reply
    reply=${reply:-$default}
    if [[ ! $reply =~ ^[0-9]+$ ]] || ((reply < 1 || reply > $#)); then
        echo "Unknown choice: $reply" >&2
        exit 2
    fi
    echo "${!reply}"
}

echo "Architecture:"
# Assigned before matching, because a failed choice inside `case $(...)` would
# not stop the script.
arch_choice=$(choose "Choose" 1 "$host_arch (this machine)" "$other_arch" "Both")
case $arch_choice in
    Both) arches=("$host_arch" "$other_arch") ;;
    "$other_arch") arches=("$other_arch") ;;
    *) arches=("$host_arch") ;;
esac

# A build for another architecture cannot be installed here, so it always
# produces a file, and "Install only" is offered only for this machine alone.
if [[ ${#arches[@]} -eq 1 && ${arches[0]} != "$host_arch" ]]; then
    target="Flatpak file"
elif [[ ${#arches[@]} -eq 2 ]]; then
    echo "Target:"
    target_choice=$(choose "Choose" 1 "Flatpak files" "Flatpak files and install")
    case $target_choice in
        "Flatpak files") target="Flatpak file" ;;
        *) target="Flatpak file and install" ;;
    esac
else
    echo "Target:"
    target=$(choose "Choose" 1 "Flatpak file" "Install only" "Flatpak file and install")
fi

echo "Build profile:"
profile_choice=$(choose "Choose" 1 \
    "Fast release: optimised, quick to rebuild" \
    "Debug: quickest to build, slower to run" \
    "Release: as published, slowest to build")
case $profile_choice in
    Debug*) profile=dev profile_dir=debug ;;
    Release*) profile=release profile_dir=release ;;
    *) profile=release-dev profile_dir=release-dev ;;
esac

if command -v flatpak >/dev/null && flatpak info org.flatpak.Builder >/dev/null 2>&1; then
    builder=(flatpak run org.flatpak.Builder)
elif command -v flatpak-builder >/dev/null; then
    builder=(flatpak-builder)
else
    echo "Install org.flatpak.Builder from Flathub, or flatpak-builder." >&2
    exit 1
fi

# Reads a top-level value from the YAML manifest, or the finish-args with the
# command first. Only the simple forms the manifest uses are understood, so no
# YAML library is needed.
manifest_value() {
    awk -v key="$1" '
        /^[^ #-]/ { in_list = 0 }
        key == "finish-args" && /^command:/ { print "--command=" $2 }
        key == "finish-args" && /^finish-args:/ { in_list = 1; next }
        in_list && /^  - / { sub(/^  - /, ""); print }
        key != "finish-args" && $0 ~ "^" key ":" {
            sub("^" key ":[ ]*", ""); gsub(/['"'"'"]/, ""); print
        }
    ' "$MANIFEST"
}

sdk=$(manifest_value sdk)/ARCH/$(manifest_value runtime-version)

# Sets up the build directory with the SDK and the Rust extension. The
# manifest has no dependency modules, so nothing is compiled here.
build_deps() {
    local arch=$1 state=.flatpak/$1
    echo "==> Preparing the SDK for $arch"
    # Missing SDKs go to whichever installation already holds this
    # architecture's SDK, and system-wide when neither does. flatpak-builder
    # only looks in the installation it is told to use.
    local ref=${sdk/ARCH/$arch} scope=()
    if ! flatpak --system info "$ref" >/dev/null 2>&1 &&
        flatpak --user info "$ref" >/dev/null 2>&1; then
        scope=(--user)
    fi
    "${builder[@]}" "${scope[@]}" --arch="$arch" --force-clean --stop-at="$MODULE" \
        --state-dir=.flatpak/builder --install-deps-from=flathub \
        "$state/build" "$MANIFEST"
    sha256sum "$MANIFEST" >"$state/manifest.sha256"
}

build_app() {
    local arch=$1 state=.flatpak/$1
    mkdir -p "$state"
    # The SDK directory is rebuilt only when the manifest changes.
    if [[ ! -f $state/build/metadata ]] || ! sha256sum --status -c "$state/manifest.sha256" 2>/dev/null; then
        build_deps "$arch"
    fi
    # Crates download into a cache shared by both architectures; compiled
    # output is kept per architecture.
    local sdk_shell=(flatpak build --share=network --filesystem="$PWD"
        --env=PATH=/app/bin:/usr/bin:/usr/lib/sdk/rust-stable/bin
        --env=CARGO_HOME="$PWD/.flatpak/cargo"
        --env=CARGO_TARGET_DIR="$PWD/$state/target"
        --env=CARGO_TERM_COLOR=always "$state/build")
    # Cargo.lock is optional until one has been generated and committed.
    local locked=()
    [[ -f Cargo.lock ]] && locked=(--locked)
    echo "==> Compiling $arch ($profile)"
    "${sdk_shell[@]}" cargo build --profile "$profile" "${locked[@]}" --workspace
    # shellcheck disable=SC2016 # $1 expands inside the sandbox.
    "${sdk_shell[@]}" sh -c '
        set -e
        install -Dm755 "$1/piopio" "$1/piopio-daemon" -t /app/bin
        install -Dm644 data/net.dunevent.Piopio.desktop -t /app/share/applications
        install -Dm644 data/net.dunevent.Piopio.metainfo.xml -t /app/share/metainfo
        install -Dm644 data/net.dunevent.Piopio.Daemon.service data/net.dunevent.Piopio.service -t /app/share/dbus-1/services
        install -Dm644 data/icons/hicolor/scalable/apps/net.dunevent.Piopio.svg -t /app/share/icons/hicolor/scalable/apps
        install -Dm644 data/icons/hicolor/symbolic/apps/net.dunevent.Piopio-symbolic.svg -t /app/share/icons/hicolor/symbolic/apps
    ' sh "$PWD/$state/target/$profile_dir"
}

make_bundle() {
    local arch=$1 bundle=$2 state=.flatpak/$1
    echo "==> Bundling $bundle"
    # build-finish can only run once on a directory, so a copy is finished and
    # the build directory stays usable for incremental builds.
    local finish=$state/finish
    rm -rf "$finish"
    cp -a --reflink=auto "$state/build" "$finish"
    # Debug info makes the bundle several times larger and slow to compress.
    flatpak build "$finish" strip --strip-debug /app/bin/piopio /app/bin/piopio-daemon

    local finish_args
    mapfile -t finish_args < <(manifest_value finish-args)
    # The manifest builds releases, so more logging is turned on here.
    finish_args+=("--env=PIOPIO_LOG=info,piopio_daemon=debug" --env=RUST_BACKTRACE=1)
    flatpak build-finish "${finish_args[@]}" "$finish" >/dev/null
    flatpak build-export --arch="$arch" "$state/repo" "$finish" >/dev/null
    mkdir -p "$(dirname "$bundle")"
    flatpak build-bundle --arch="$arch" --runtime-repo="$RUNTIME_REPO" \
        "$state/repo" "$bundle" "$APP_ID"
    rm -rf "$finish"
}

for arch in "${arches[@]}"; do
    if [[ $arch != "$host_arch" ]] && ! compgen -G "/proc/sys/fs/binfmt_misc/*$arch*" >/dev/null; then
        echo "Building for $arch on $host_arch needs QEMU user emulation registered" >&2
        echo "with binfmt_misc (qemu-user-static or qemu-user-binfmt)." >&2
        exit 1
    fi
done

outputs=()
for arch in "${arches[@]}"; do
    build_app "$arch"

    install=false
    file=true
    if [[ $arch == "$host_arch" ]]; then
        [[ $target == "Flatpak file" ]] || install=true
        [[ $target == "Install only" ]] && file=false
    fi

    if $file; then
        bundle=$DIST/$APP_ID-$arch.flatpak
        make_bundle "$arch" "$bundle"
        outputs+=("$bundle")
    fi
    if $install; then
        if $file; then
            source_bundle=$bundle
        else
            source_bundle=.flatpak/$arch/install.flatpak
            make_bundle "$arch" "$source_bundle"
        fi
        echo "==> Installing $arch for this user"
        flatpak install --user --reinstall -y "$source_bundle"
        $file || rm -f "$source_bundle"
    fi
done

echo
for output in "${outputs[@]}"; do
    echo "Wrote $output"
done
