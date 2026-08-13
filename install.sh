#!/bin/sh
set -eu

repository=${KKAGENT_REPOSITORY:-Ken-u/kkagent}
script_path=$0
case "$script_path" in
  */*) ;;
  *)
    resolved_script=$(command -v "$script_path" 2>/dev/null || true)
    if [ -n "$resolved_script" ]; then
      script_path=$resolved_script
    fi
    ;;
esac
if [ -n "${KKAGENT_INSTALL_DIR:-}" ]; then
  install_dir=$KKAGENT_INSTALL_DIR
elif [ "${script_path##*/}" = kkagent-update ]; then
  install_dir=$(CDPATH= cd "$(dirname "$script_path")" && pwd)
elif [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
  install_dir=/usr/local/bin
else
  install_dir=${HOME:?HOME is required}/.local/bin
fi
version=${KKAGENT_VERSION:-latest}
if [ -n "${KKAGENT_RELEASE_BASE_URL:-}" ]; then
  base_url=$KKAGENT_RELEASE_BASE_URL
elif [ "$version" = latest ]; then
  base_url=https://github.com/$repository/releases/latest/download
else
  base_url=https://github.com/$repository/releases/download/v${version#v}
fi

download() {
  url=$1
  output=$2
  if command -v curl >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
      --retry 3 --output "$output" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget --https-only --quiet --tries=3 --output-document="$output" "$url"
  else
    echo "curl or wget is required" >&2
    exit 1
  fi
}

case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

if [ -n "${KKAGENT_TARGET:-}" ]; then
  target=$KKAGENT_TARGET
else
  case "$(uname -s)" in
    Linux) target="$arch-unknown-linux-musl" ;;
    Darwin) target="$arch-apple-darwin" ;;
    *) echo "unsupported operating system: $(uname -s)" >&2; exit 1 ;;
  esac
fi
archive="kkagent-$target.tar.gz"
temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/kkagent-install.XXXXXX")
new_binary=
new_updater=
cleanup() {
  if [ -n "$new_binary" ] && [ -f "$new_binary" ]; then
    rm -f "$new_binary"
  fi
  if [ -n "$new_updater" ] && [ -f "$new_updater" ]; then
    rm -f "$new_updater"
  fi
  if [ -n "${temp_dir:-}" ] && [ -d "$temp_dir" ]; then
    rm -rf "$temp_dir"
  fi
}
trap cleanup EXIT HUP INT TERM

echo "Downloading kkagent ${version} for ${target}..."
download "$base_url/$archive" "$temp_dir/$archive"
download "$base_url/SHA256SUMS" "$temp_dir/SHA256SUMS"
expected=$(awk -v file="$archive" '$2 == file { print $1 }' "$temp_dir/SHA256SUMS")
if [ -z "$expected" ]; then
  echo "$archive is missing from SHA256SUMS" >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$temp_dir/$archive" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$temp_dir/$archive" | awk '{ print $1 }')
else
  echo "sha256sum or shasum is required" >&2
  exit 1
fi
if [ "$actual" != "$expected" ]; then
  echo "checksum verification failed for $archive" >&2
  exit 1
fi

mkdir -p "$temp_dir/package" "$install_dir"
tar -C "$temp_dir/package" -xzf "$temp_dir/$archive"
new_binary="$install_dir/kkagent.new.$$"
cp "$temp_dir/package/kkagent" "$new_binary"
chmod 0755 "$new_binary"
mv "$new_binary" "$install_dir/kkagent"
ln -sf "kkagent" "$install_dir/kk"

# New releases carry the installer inside the checksum-verified archive. Keep
# compatibility with older archives and `curl | sh` by falling back to the
# script currently being run, then to the canonical repository copy.
installer_source=
if [ -f "$temp_dir/package/install.sh" ]; then
  installer_source=$temp_dir/package/install.sh
elif [ -f "$script_path" ]; then
  case "${script_path##*/}" in
    sh|dash|bash|zsh) ;;
    *) installer_source=$script_path ;;
  esac
fi
if [ -z "$installer_source" ]; then
  installer_url=${KKAGENT_INSTALLER_URL:-https://raw.githubusercontent.com/$repository/main/install.sh}
  installer_source=$temp_dir/install.sh
  echo "Downloading reusable updater..."
  download "$installer_url" "$installer_source"
fi
new_updater="$install_dir/kkagent-update.new.$$"
cp "$installer_source" "$new_updater"
chmod 0755 "$new_updater"
mv "$new_updater" "$install_dir/kkagent-update"

echo "Installed kkagent to $install_dir/kkagent and linked kk -> kkagent"
echo "Installed updater to $install_dir/kkagent-update; run kkagent-update to upgrade"
"$install_dir/kkagent" --version

case ":${PATH:-}:" in
  *":$install_dir:"*) ;;
  *) echo "Add $install_dir to PATH, then open a new shell." ;;
esac
