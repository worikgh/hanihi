#!/usr/bin/env bash
set -euo pipefail

release_dir="./target/release"
bin_dir="${HOME}/.local/bin"

mkdir -p "$bin_dir"

# Release filename -> destination filename
declare -A binaries=(
  [analyse]="hānihi-analyse"
  [hanihi-cli]="hānihi-cli"
)

for source_name in "${!binaries[@]}"; do
  source="${release_dir}/${source_name}"
  destination="${bin_dir}/${binaries[$source_name]}"

  if [[ ! -f "$source" ]]; then
    echo "Skipping missing file: $source" >&2
    continue
  fi

  # Copy if the destination does not exist or the source is newer.
  if [[ ! -e "$destination" || "$source" -nt "$destination" ]]; then
    install -m 755 "$source" "$destination"
    echo "Installed: $destination"
  else
    echo "Up to date: $destination"
  fi
done
