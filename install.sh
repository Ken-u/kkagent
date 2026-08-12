#!/bin/sh
set -eu

repository=${KKAGENT_REPOSITORY:-Ken-u/kkagent}
if [ -n "${KKAGENT_INSTALL_DIR:-}" ]; then
  install_dir=$KKAGENT_INSTALL_DIR
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
cleanup() {
  if [ -n "$new_binary" ] && [ -f "$new_binary" ]; then
    rm -f "$new_binary"
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
echo "Installed kkagent to $install_dir/kkagent and linked kk -> kkagent"
"$install_dir/kkagent" --version

case ":${PATH:-}:" in
  *":$install_dir:"*) ;;
  *) echo "Add $install_dir to PATH, then open a new shell." ;;
esac
