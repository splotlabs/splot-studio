#!/usr/bin/env bash
# SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
# SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>
#
# Builds the FFmpeg that Splot Studio ships: dav1d, then ffmpeg and ffprobe
# linked statically against it, from the archives pinned in versions.env.
#
#   native/ffmpeg/build.sh linux-x86_64|windows-x86_64|macos-arm64
#
# Runs natively on the target's OS (Windows: an MSYS2 UCRT64 shell). Writes
# out/<target>/: ffmpeg, ffprobe, MANIFEST.json and licenses/. Sources and
# build trees go to work/. See README.md for the configuration and why.
#
# Written for bash 3.2 as well (macOS /bin/bash): no mapfile, no
# associative arrays, no empty-array expansion under `set -u`.
set -euo pipefail

usage() {
    echo "usage: $0 linux-x86_64|windows-x86_64|macos-arm64" >&2
    exit 2
}

die() {
    echo "build.sh: $*" >&2
    exit 1
}

[[ $# -eq 1 ]] || usage
target=$1
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=versions.env disable=SC1091 # plain shellcheck does not follow it
source "$here/versions.env"

case $target in
linux-x86_64)
    [[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] ||
        die "$target is built on Linux x86_64"
    exe=
    cc=gcc
    jobs=$(nproc)
    ;;
windows-x86_64)
    [[ ${MSYSTEM:-} == UCRT64 && $(uname -m) == x86_64 ]] ||
        die "$target is built in an MSYS2 UCRT64 shell"
    exe=.exe
    cc=gcc
    jobs=$(nproc)
    ;;
macos-arm64)
    [[ $(uname -s) == Darwin && $(uname -m) == arm64 ]] ||
        die "$target is built on macOS arm64"
    exe=
    cc=clang
    jobs=$(sysctl -n hw.ncpu)
    # The oldest macOS the executables run on; clang and meson read it.
    export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-12.0}
    ;;
*)
    usage
    ;;
esac

downloads=$here/work/downloads
work=$here/work/$target
prefix=$work/prefix
out=$here/out/$target

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# Downloads $1 into work/downloads unless a copy with SHA-256 $2 is there,
# and refuses a download with any other hash.
fetch() {
    local url=$1 want=$2 file
    file=$downloads/${url##*/}
    if [[ -f $file && $(sha256 "$file") == "$want" ]]; then
        return
    fi
    rm -f "$file" "$file.part"
    curl --fail --location --proto '=https' --tlsv1.2 --retry 3 \
        --silent --show-error --output "$file.part" "$url"
    local got
    got=$(sha256 "$file.part")
    if [[ $got != "$want" ]]; then
        rm -f "$file.part"
        die "SHA-256 of $url is $got, versions.env pins $want"
    fi
    mv "$file.part" "$file"
}

# The first line of what "$@" prints, without carriage returns.
first_line() {
    local text
    text=$("$@" 2>&1) || true
    text=${text%%$'\n'*}
    printf '%s' "${text//$'\r'/}"
}

json_str() {
    local s=$1
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    s=${s//$'\t'/\\t}
    s=${s//$'\r'/}
    s=${s//$'\n'/\\n}
    printf '"%s"' "$s"
}

json_array() {
    local first=1 item
    printf '['
    for item in "$@"; do
        [[ $first == 1 ]] || printf ', '
        first=0
        json_str "$item"
    done
    printf ']'
}

# The lines of stdin as a JSON array.
json_lines() {
    local first=1 line
    printf '['
    while IFS= read -r line; do
        line=${line//$'\r'/}
        [[ -n $line ]] || continue
        [[ $first == 1 ]] || printf ', '
        first=0
        json_str "$line"
    done
    printf ']'
}

mkdir -p "$downloads"
fetch "$FFMPEG_URL" "$FFMPEG_SHA256"
fetch "$DAV1D_URL" "$DAV1D_SHA256"

rm -rf "$work" "$out"
mkdir -p "$work/src" "$work/build-ffmpeg" "$out/licenses/ffmpeg" "$out/licenses/dav1d"
tar -xJf "$downloads/${FFMPEG_URL##*/}" -C "$work/src"
tar -xJf "$downloads/${DAV1D_URL##*/}" -C "$work/src"
ffmpeg_src=$work/src/ffmpeg-$FFMPEG_VERSION
dav1d_src=$work/src/dav1d-$DAV1D_VERSION
[[ -x $ffmpeg_src/configure && -f $dav1d_src/meson.build ]] ||
    die "the archives did not unpack to ffmpeg-$FFMPEG_VERSION and dav1d-$DAV1D_VERSION"

export CC=$cc

# dav1d: a static library with its own runtime CPU dispatch (asm on), for
# 8-bit and high bit depth (10/12-bit) streams; no tools, tests or docs.
dav1d_meson=(
    --prefix="$prefix"
    --libdir=lib
    --buildtype=release
    --default-library=static
    "-Dbitdepths=8,16"
    -Denable_asm=true
    -Denable_tools=false
    -Denable_examples=false
    -Denable_tests=false
    -Denable_docs=false
)
meson setup "$work/build-dav1d" "$dav1d_src" "${dav1d_meson[@]}"
meson compile -C "$work/build-dav1d"
meson install -C "$work/build-dav1d"

# FFmpeg: nothing enabled unless listed, nothing found by autodetection.
# Every component the engine's invocations use (crates/studio-engine/src/media)
# and the decoders the release supports; configure adds what these select
# (for example ffmpeg's own null/format/trim filters and the decoders' bitstream
# filters). The executables and libraries are linked statically.
ffmpeg_configure=(
    --cc="$cc"
    --extra-version=splot-studio
    --disable-everything
    --disable-autodetect
    --disable-network
    --disable-doc
    --disable-ffplay
    --disable-avdevice
    --disable-swresample
    --enable-static
    --disable-shared
    --pkg-config-flags=--static
    --enable-libdav1d
    "--enable-decoder=h264,hevc,vp8,vp9,mpeg1video,mpeg2video,mpeg4,prores,dnxhd,ffv1,mjpeg,rawvideo,libdav1d"
    "--enable-parser=h264,hevc,vp8,vp9,mpegvideo,mpeg4video,av1,dnxhd,mjpeg,ffv1"
    "--enable-demuxer=mov,matroska,mpegts,avi"
    --enable-muxer=yuv4mpegpipe
    --enable-encoder=wrapped_avframe
    "--enable-protocol=file,pipe"
    "--enable-filter=select,scale,format,null"
)
case $target in
linux-x86_64)
    # Threads are autodetected, so --disable-autodetect needs them named.
    ffmpeg_configure+=(--enable-pthreads --toolchain=hardened)
    ;;
windows-x86_64)
    # -static: libgcc and any winpthread go into the executable, not a DLL.
    ffmpeg_configure+=(--enable-w32threads --extra-ldexeflags=-static)
    ;;
macos-arm64)
    ffmpeg_configure+=(--enable-pthreads)
    ;;
esac
(
    cd "$work/build-ffmpeg"
    # Only the dav1d built above is visible to pkg-config.
    PKG_CONFIG_LIBDIR=$prefix/lib/pkgconfig PKG_CONFIG_PATH='' \
        "$ffmpeg_src/configure" "${ffmpeg_configure[@]}"
)
make -C "$work/build-ffmpeg" -j"$jobs"
for tool in ffmpeg ffprobe; do
    cp "$work/build-ffmpeg/$tool$exe" "$out/$tool$exe"
done
ffmpeg=$out/ffmpeg$exe
ffprobe=$out/ffprobe$exe

# --- Checks: any failure here means the pair must not be shipped. ---

ffmpeg_line=$(first_line "$ffmpeg" -version)
ffprobe_line=$(first_line "$ffprobe" -version)
want_version=$FFMPEG_VERSION-splot-studio
[[ $ffmpeg_line == "ffmpeg version $want_version "* ]] ||
    die "ffmpeg -version says: $ffmpeg_line"
[[ $ffprobe_line == "ffprobe version $want_version "* ]] ||
    die "ffprobe -version says: $ffprobe_line"

buildconf=$("$ffmpeg" -hide_banner -buildconf | tr -d '\r')
if grep -Eq -- '--enable-(gpl|nonfree|version3)' <<<"$buildconf"; then
    die "the configuration enables GPL, nonfree or version 3 code"
fi
licence=$("$ffmpeg" -hide_banner -L | tr -d '\r')
if ! grep -q 'GNU Lesser General Public' <<<"$licence" || ! grep -q 'version 2.1' <<<"$licence"; then
    die "ffmpeg -L does not report LGPL 2.1: $licence"
fi

# Both start with an empty environment: no PATH, no library search path.
for tool in "$ffmpeg" "$ffprobe"; do
    if [[ $target == windows-x86_64 ]]; then
        env -i SYSTEMROOT="${SYSTEMROOT:-C:\\Windows}" "$tool" -version >/dev/null ||
            die "$tool does not start with an empty environment"
    else
        env -i "$tool" -version >/dev/null ||
            die "$tool does not start with an empty environment"
    fi
done

# The shared libraries an executable needs, one per line.
needs() {
    case $target in
    linux-x86_64)
        readelf -d "$1" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'
        ;;
    windows-x86_64)
        objdump -p "$1" | sed -n 's/.*DLL Name: *//p' | tr -d '\r'
        ;;
    macos-arm64)
        otool -L "$1" | tail -n +2 | awk '{print $1}'
        ;;
    esac
}

# Only libraries every supported installation of the OS has.
is_system_library() {
    case $target in
    linux-x86_64)
        case $1 in
        libc.so.6 | libm.so.6 | libpthread.so.0 | libdl.so.2) return 0 ;;
        esac
        ;;
    windows-x86_64)
        case $(tr '[:upper:]' '[:lower:]' <<<"$1") in
        kernel32.dll | advapi32.dll | bcrypt.dll | shell32.dll | ole32.dll | psapi.dll | user32.dll) return 0 ;;
        api-ms-win-crt-*.dll | api-ms-win-core-*.dll) return 0 ;;
        esac
        ;;
    macos-arm64)
        case $1 in
        /usr/lib/* | /System/Library/*) return 0 ;;
        esac
        ;;
    esac
    return 1
}

for tool in "$ffmpeg" "$ffprobe"; do
    # Read into a variable first, so a failing inspection tool stops the build
    # instead of yielding an empty, passing list.
    libraries=$(needs "$tool")
    [[ -n $libraries ]] || die "found no shared libraries in ${tool##*/}; is the inspection tool working?"
    while IFS= read -r library; do
        is_system_library "$library" ||
            die "${tool##*/} needs $library, which is not a system library"
    done <<<"$libraries"
done

# The newest OS interface an executable uses.
os_floor() {
    case $target in
    linux-x86_64)
        printf 'GLIBC symbols up to %s' \
            "$(objdump -T "$1" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -n 1)"
        ;;
    windows-x86_64)
        printf 'subsystem version %s' \
            "$(objdump -p "$1" | sed -n 's/^MajorSubsystemVersion[[:space:]]*//p' | tr -d '\r')"
        ;;
    macos-arm64)
        printf 'minos %s' "$(otool -l "$1" | awk '/LC_BUILD_VERSION/ {f=1} f && $1 == "minos" {print $2; exit}')"
        ;;
    esac
}

# --- Licences and the replacement note. ---

cp "$ffmpeg_src/LICENSE.md" "$ffmpeg_src/COPYING.LGPLv2.1" "$ffmpeg_src/CREDITS" \
    "$out/licenses/ffmpeg/"
cp "$dav1d_src/COPYING" "$out/licenses/dav1d/COPYING"
# FFmpeg's LICENSE.md asks binary distributions to credit the IJG for the
# libjpeg-derived DCT files, when they are built in.
ijg=
for object in jrevdct jfdctint jfdctfst; do
    if [[ -f $work/build-ffmpeg/libavcodec/$object.o ]]; then
        ijg="${ijg:+$ijg, }libavcodec/$object.c"
    fi
done
if [[ -n $ijg ]]; then
    ijg="This software is based in part on the work of the Independent JPEG Group.
FFmpeg's libjpeg-derived files built in here ($ijg) are used unchanged."
fi

studio_commit=$(git -C "$here" rev-parse HEAD 2>/dev/null || echo unknown)
cat >"$out/licenses/SOURCE.md" <<EOF
# FFmpeg and dav1d in Splot Studio

\`ffmpeg$exe\` and \`ffprobe$exe\` are FFmpeg $FFMPEG_VERSION, licensed under the
GNU Lesser General Public License version 2.1 or later (\`ffmpeg/COPYING.LGPLv2.1\`,
\`ffmpeg/LICENSE.md\`). They contain dav1d $DAV1D_VERSION, licensed under the BSD
2-clause licence (\`dav1d/COPYING\`). Both are unmodified upstream releases,
separate programs from Splot Studio, and are not covered by Splot Studio's licence.
${ijg:+
$ijg
}
## Source

- FFmpeg: $FFMPEG_URL
  SHA-256 $FFMPEG_SHA256
- dav1d: $DAV1D_URL
  SHA-256 $DAV1D_SHA256
- Build script and configuration: \`native/ffmpeg/\` in the Splot Studio source,
  commit $studio_commit. \`MANIFEST.json\` beside the executables records the
  exact configure and meson lines and the toolchain.

## Rebuilding and replacing

Run \`native/ffmpeg/build.sh $target\` from the Splot Studio source (see
\`native/ffmpeg/README.md\` for the tools it needs), or build FFmpeg your own way,
modified or not. Replace both executables together: the engine uses them only
as a pair that reports the same version, found beside the engine executable or
in its \`ffmpeg/\` directory. The engine needs the demuxers mov, matroska, mpegts
and avi, the protocols file and pipe, the yuv4mpegpipe muxer with the
wrapped_avframe encoder, the select and scale filters, and the decoders for your
files. On macOS, replacing files inside a signed application invalidates its
signature; re-sign it (for example \`codesign --force --deep --sign - <app>\`).
EOF

# --- MANIFEST.json ---

ffmpeg_list() {
    "$ffmpeg" -hide_banner "$1" 2>/dev/null | tr -d '\r'
}
# Rows after the "------" rule: "<flags> <name> <description>".
after_rule() { awk 'f && NF {print $2} /^ *-+ *$/ {f=1}'; }

toolchain_entry() {
    printf '    %s: %s' "$(json_str "$1")" "$(json_str "$2")"
}

executable_entry() {
    local file=$1
    printf '    %s: {"sha256": %s, "bytes": %s, "needs": %s, "os_floor": %s}' \
        "$(json_str "${file##*/}")" "$(json_str "$(sha256 "$file")")" \
        "$(wc -c <"$file" | tr -d ' ')" "$(needs "$file" | json_lines)" \
        "$(json_str "$(os_floor "$file")")"
}

case $target in
linux-x86_64) os_line=$(first_line ldd --version) ;;
windows-x86_64) os_line="MSYS2 $(first_line uname -r), MSYSTEM=$MSYSTEM" ;;
macos-arm64) os_line="macOS $(sw_vers -productVersion), SDK $(xcrun --show-sdk-version), MACOSX_DEPLOYMENT_TARGET=$MACOSX_DEPLOYMENT_TARGET" ;;
esac

{
    printf '{\n'
    printf '  "target": %s,\n' "$(json_str "$target")"
    printf '  "built_utc": %s,\n' "$(json_str "$(date -u +%Y-%m-%dT%H:%M:%SZ)")"
    printf '  "host": %s,\n' "$(json_str "$(uname -srm)")"
    printf '  "studio_commit": %s,\n' "$(json_str "$studio_commit")"
    printf '  "ffmpeg": {\n'
    printf '    "version": %s,\n' "$(json_str "$FFMPEG_VERSION")"
    printf '    "url": %s,\n' "$(json_str "$FFMPEG_URL")"
    printf '    "sha256": %s,\n' "$(json_str "$FFMPEG_SHA256")"
    printf '    "configure": %s,\n' "$(json_array "${ffmpeg_configure[@]}")"
    printf '    "version_line": %s,\n' "$(json_str "$ffmpeg_line")"
    printf '    "buildconf": %s,\n' "$(json_lines <<<"$buildconf")"
    printf '    "licence": "LGPL-2.1-or-later"\n'
    printf '  },\n'
    printf '  "dav1d": {\n'
    printf '    "version": %s,\n' "$(json_str "$DAV1D_VERSION")"
    printf '    "url": %s,\n' "$(json_str "$DAV1D_URL")"
    printf '    "sha256": %s,\n' "$(json_str "$DAV1D_SHA256")"
    printf '    "meson": %s,\n' "$(json_array "${dav1d_meson[@]}")"
    printf '    "licence": "BSD-2-Clause"\n'
    printf '  },\n'
    printf '  "components": {\n'
    printf '    "decoders": %s,\n' "$(ffmpeg_list -decoders | after_rule | json_lines)"
    printf '    "encoders": %s,\n' "$(ffmpeg_list -encoders | after_rule | json_lines)"
    printf '    "demuxers": %s,\n' "$(ffmpeg_list -demuxers | after_rule | json_lines)"
    printf '    "muxers": %s,\n' "$(ffmpeg_list -muxers | after_rule | json_lines)"
    printf '    "protocols": %s,\n' "$(ffmpeg_list -protocols | sed -n '2,$s/^ *//p' | json_lines)"
    printf '    "filters": %s,\n' "$(ffmpeg_list -filters | awk '$3 ~ /->/ {print $2}' | json_lines)"
    printf '    "bitstream_filters": %s\n' "$(ffmpeg_list -bsfs | tail -n +2 | json_lines)"
    printf '  },\n'
    printf '  "toolchain": {\n'
    toolchain_entry cc "$(first_line "$cc" --version)"
    printf ',\n'
    toolchain_entry nasm "$(if command -v nasm >/dev/null 2>&1; then first_line nasm -v; else echo none; fi)"
    printf ',\n'
    toolchain_entry meson "$(first_line meson --version)"
    printf ',\n'
    toolchain_entry ninja "$(first_line ninja --version)"
    printf ',\n'
    toolchain_entry pkg-config "$(first_line pkg-config --version)"
    printf ',\n'
    toolchain_entry make "$(first_line make --version)"
    printf ',\n'
    toolchain_entry os "$os_line"
    printf '\n  },\n'
    printf '  "executables": {\n'
    executable_entry "$ffmpeg"
    printf ',\n'
    executable_entry "$ffprobe"
    printf '\n  }\n'
    printf '}\n'
} >"$out/MANIFEST.json"

echo "built $out:"
echo "  $ffmpeg_line"
for tool in "$ffmpeg" "$ffprobe"; do
    echo "  ${tool##*/}: $(sha256 "$tool"), needs $(needs "$tool" | tr '\n' ' ')"
done
