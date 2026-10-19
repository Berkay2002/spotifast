#!/usr/bin/env bash
# Updates assets/i18n with fastframe-i18n's script, from the checkout Cargo
# already has. Requires GNU gettext tools with Rust support; normal Cargo
# builds do not. Pass --check to fail on a stale template instead.
set -euo pipefail
cd "$(dirname "$0")/../.."
# On Windows the path arrives JSON-escaped (fastframe-i18n\\Cargo.toml):
# match either separator and turn each escaped backslash into a slash.
crate=$(cargo metadata --format-version 1 --locked |
    grep -Eo '"manifest_path":"[^"]*fastframe-i18n(/|\\\\)Cargo.toml"' | head -n1 |
    sed 's/^"manifest_path":"//; s/Cargo.toml"$//; s/\\\\/\//g') ||
    { echo "fastframe-i18n is missing from cargo metadata" >&2; exit 1; }
exec bash "$crate/scripts/update-translations.sh" --package Spotifast --domain spotifast \
    --bugs 'https://github.com/Berkay2002/spotifast/issues/new?template=translation.yml' --keyword gettext "$@"
