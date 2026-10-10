#!/bin/bash
# dropin v0.2.0 installer — rootless, installs to ~/.local/bin
set -e
cd "$(dirname "$0")"
mkdir -p ~/.local/bin
cp dropin dropin-gui ~/.local/bin/
chmod +x ~/.local/bin/dropin ~/.local/bin/dropin-gui
export PATH="$HOME/.local/bin:$PATH"
~/.local/bin/dropin integrate
echo ""
echo "dropin installed. Open 'dropin' from your app menu, or double-click any installer file."
