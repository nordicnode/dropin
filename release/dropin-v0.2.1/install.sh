#!/bin/bash
# dropin v0.2.1 installer — rootless, installs to ~/.local/bin
set -e
cd "$(dirname "$0")"
mkdir -p ~/.local/bin
cp dropin ~/.local/bin/
chmod +x ~/.local/bin/dropin
echo ""
echo "dropin v0.2.1 installed to ~/.local/bin/dropin"
echo "Run 'dropin --help' to see what it can do."
echo "For Windows games: 'dropin setup-wine' or 'dropin setup-proton'."
