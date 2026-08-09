#!/bin/sh
set -eu

repository=${KKAGENT_REPOSITORY:-bianjinchen/kkagent}
install_dir=${KKAGENT_INSTALL_DIR:-/usr/local/bin}
base_url=${KKAGENT_RELEASE_BASE_URL:-https://github.com/$repository/releases/latest/download}

case "$(uname -s)" in
  Linux) os=unknown-linux-gnu ;;
  Darwin) os=apple-darwin ;;
  *) echo "unsupported operating system: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

target="$arch-$os"
archive="kkagent-$target.tar.gz"
temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/kkagent-install.XXXXXX")
cleanup() {
  if [ -n "${temp_dir:-}" ] && [ -d "$temp_dir" ]; then
    rm -rf "$temp_dir"
  fi
}
trap cleanup EXIT HUP INT TERM

curl --fail --location --retry 3 --output "$temp_dir/$archive" "$base_url/$archive"
curl --fail --location --retry 3 --output "$temp_dir/SHA256SUMS" "$base_url/SHA256SUMS"
expected=$(awk -v file="$archive" '$2 == file { print $1 }' "$temp_dir/SHA256SUMS")
if [ -z "$expected" ]; then
  echo "$archive is missing from SHA256SUMS" >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$temp_dir/$archive" | awk '{ print $1 }')
else
  actual=$(shasum -a 256 "$temp_dir/$archive" | awk '{ print $1 }')
fi
if [ "$actual" != "$expected" ]; then
  echo "checksum verification failed for $archive" >&2
  exit 1
fi

mkdir -p "$temp_dir/package" "$install_dir"
tar -C "$temp_dir/package" -xzf "$temp_dir/$archive"
install -m 0755 "$temp_dir/package/kkagent" "$install_dir/kkagent.new"
mv "$install_dir/kkagent.new" "$install_dir/kkagent"
echo "Installed kkagent to $install_dir/kkagent"
"$install_dir/kkagent" --version
