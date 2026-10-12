#!/usr/bin/env bash
set -euo pipefail

repository=capsulerun/prober
release_url=${PROBER_RELEASE_URL:-https://github.com/$repository/releases/download}
unreleased_version=0.0.0

fail() {
  echo "::error::$1"
  exit 1
}

native_path() {
  if command -v cygpath > /dev/null; then
    cygpath -w "$1"
  else
    echo "$1"
  fi
}

unix_path() {
  if command -v cygpath > /dev/null; then
    cygpath -u "$1"
  else
    echo "$1"
  fi
}

sha256() {
  if command -v sha256sum > /dev/null; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

action_path=$(unix_path "$GITHUB_ACTION_PATH")
version=${INPUT_VERSION:-}

if [ -z "$version" ]; then
  version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$action_path/Cargo.toml" | head -n 1)
fi

version=${version#v}

if [ "$version" = "$unreleased_version" ]; then
  echo "::notice::This Action's checkout is not a release: building Prober from source"
  (cd "$action_path" && cargo build --quiet --locked -p prober)
  native_path "$action_path/target/debug" >> "$GITHUB_PATH"
  exit 0
fi

binary=prober
extension=tar.xz

case "$RUNNER_OS/$RUNNER_ARCH" in
  Linux/X64)
    target=x86_64-unknown-linux-gnu
    if ldd --version 2>&1 | grep -qi musl; then
      target=x86_64-unknown-linux-musl
    fi
    ;;
  Linux/ARM64) target=aarch64-unknown-linux-gnu ;;
  macOS/X64) target=x86_64-apple-darwin ;;
  macOS/ARM64) target=aarch64-apple-darwin ;;
  Windows/X64)
    target=x86_64-pc-windows-msvc
    binary=prober.exe
    extension=zip
    ;;
  *) fail "Prober has no build for $RUNNER_OS on $RUNNER_ARCH" ;;
esac

tool_cache=$(unix_path "${RUNNER_TOOL_CACHE:-$RUNNER_TEMP}")
install_directory="$tool_cache/prober/$version/$target"

if [ -x "$install_directory/$binary" ]; then
  echo "Prober $version found in the tool cache"
  native_path "$install_directory" >> "$GITHUB_PATH"
  exit 0
fi

download_directory=$(mktemp -d)
archive="prober-$target.$extension"

for file in "$archive" "$archive.sha256"; do
  curl --proto '=https,file' --tlsv1.2 --fail --silent --show-error --location --retry 3 \
    --output "$download_directory/$file" "$release_url/v$version/$file" \
    || fail "cannot download $file of Prober $version"
done

expected_checksum=$(cut -d ' ' -f 1 "$download_directory/$archive.sha256")
actual_checksum=$(sha256 "$download_directory/$archive")

if [ "$expected_checksum" != "$actual_checksum" ]; then
  fail "$archive does not match its checksum: expected $expected_checksum, got $actual_checksum"
fi

if [ "$VERIFY_ATTESTATION" = "true" ]; then
  command -v gh > /dev/null \
    || fail "gh is needed to verify the attestation of $archive; install it, or set verify-attestation: false to check its checksum only"
  trusted_root="$action_path/action/trusted_root.jsonl"

  if ! { [ -f "$trusted_root" ] \
    && gh attestation verify "$download_directory/$archive" --repo "$repository" \
      --custom-trusted-root "$trusted_root" > /dev/null 2>&1; }; then
    gh attestation verify "$download_directory/$archive" --repo "$repository" > /dev/null \
      || fail "$archive has no valid attestation from $repository"
  fi
else
  echo "::warning::The attestation of $archive is not verified (verify-attestation: false), only its checksum"
fi

extracted_directory="$download_directory/extracted"
mkdir -p "$extracted_directory"

if [ "$extension" = zip ]; then
  unzip -q "$download_directory/$archive" -d "$extracted_directory"
else
  tar -xJf "$download_directory/$archive" -C "$extracted_directory"
fi

extracted_binary=$(find "$extracted_directory" -type f -name "$binary" | head -n 1)
[ -n "$extracted_binary" ] || fail "$archive holds no $binary"
mkdir -p "$install_directory"
mv "$extracted_binary" "$install_directory/$binary"
chmod +x "$install_directory/$binary"
rm -rf "$download_directory"
native_path "$install_directory" >> "$GITHUB_PATH"
echo "Prober $version installed for $target"
