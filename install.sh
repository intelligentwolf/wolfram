#!/bin/sh
set -e

# Wolfram installer — builds release binary and installs as a systemd service.

INSTALL_BIN="/usr/local/bin/wolfram"
SERVICE_FILE="/etc/systemd/system/wolfram.service"
CONFIG_DIR="/etc/wolfram"
DATA_DIR="/var/lib/wolfram"
CHECKPOINT_DIR="$DATA_DIR/checkpoints"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Colours for output (disabled if not a terminal).
if [ -t 1 ]; then
    GREEN='\033[0;32m'
    RED='\033[0;31m'
    YELLOW='\033[1;33m'
    NC='\033[0m'
else
    GREEN=''
    RED=''
    YELLOW=''
    NC=''
fi

info()  { printf "${GREEN}[+]${NC} %s\n" "$1"; }
warn()  { printf "${YELLOW}[!]${NC} %s\n" "$1"; }
error() { printf "${RED}[x]${NC} %s\n" "$1"; exit 1; }

# Must be root.
if [ "$(id -u)" -ne 0 ]; then
    error "This script must be run as root (sudo ./install.sh)"
fi

# Check for cargo.
if ! command -v cargo >/dev/null 2>&1; then
    error "cargo not found — install Rust first: https://rustup.rs"
fi

# Build release binary.
info "Building release binary..."
cd "$SCRIPT_DIR"
sudo -u "${SUDO_USER:-$(logname)}" cargo build --release 2>&1
BINARY="$SCRIPT_DIR/target/release/wolfram"

if [ ! -f "$BINARY" ]; then
    error "Build failed — binary not found at $BINARY"
fi

# Stop existing service if running.
if systemctl is-active --quiet wolfram 2>/dev/null; then
    info "Stopping existing wolfram service..."
    systemctl stop wolfram
fi

# Install binary.
info "Installing binary to $INSTALL_BIN"
install -m 755 "$BINARY" "$INSTALL_BIN"

# Create config directory and install default config if not present.
if [ ! -d "$CONFIG_DIR" ]; then
    info "Creating config directory $CONFIG_DIR"
    mkdir -p "$CONFIG_DIR"
fi

if [ ! -f "$CONFIG_DIR/config.toml" ]; then
    info "Installing default config to $CONFIG_DIR/config.toml"
    install -m 644 "$SCRIPT_DIR/config.example.toml" "$CONFIG_DIR/config.toml"
else
    warn "Config file already exists at $CONFIG_DIR/config.toml — not overwriting"
fi

# Create data directories.
info "Creating data directories"
mkdir -p "$DATA_DIR"
mkdir -p "$CHECKPOINT_DIR"

# Install systemd service.
info "Installing systemd service"
install -m 644 "$SCRIPT_DIR/wolfram.service" "$SERVICE_FILE"

# Reload systemd.
info "Reloading systemd daemon"
systemctl daemon-reload

# Enable the service (start on boot).
info "Enabling wolfram service"
systemctl enable wolfram

# Check kernel features.
echo ""
info "Checking kernel features..."

if [ -f /sys/kernel/mm/page_idle/bitmap ]; then
    info "  Idle page tracking: available"
else
    warn "  Idle page tracking: NOT available (need CONFIG_IDLE_PAGE_TRACKING)"
    warn "  Wolfram will still work but with reduced accuracy"
fi

if [ -f /sys/module/zswap/parameters/enabled ]; then
    ZSWAP=$(cat /sys/module/zswap/parameters/enabled)
    if [ "$ZSWAP" = "Y" ]; then
        info "  zswap: enabled"
    else
        warn "  zswap: disabled — compressed pages will go to disk swap"
        warn "  Enable with: echo Y > /sys/module/zswap/parameters/enabled"
        warn "  To persist, add 'zswap.enabled=1' to kernel command line"
    fi
else
    warn "  zswap: not available in this kernel"
fi

if command -v criu >/dev/null 2>&1; then
    info "  CRIU: available ($(criu --version 2>&1 | head -1))"
else
    info "  CRIU: not installed (optional — needed for process hibernation)"
fi

# Done.
echo ""
info "Installation complete!"
echo ""
echo "  Binary:   $INSTALL_BIN"
echo "  Config:   $CONFIG_DIR/config.toml"
echo "  Service:  $SERVICE_FILE"
echo "  Data:     $DATA_DIR"
echo ""
echo "  Edit config:    sudo nano $CONFIG_DIR/config.toml"
echo "  Start now:      sudo systemctl start wolfram"
echo "  View logs:      sudo journalctl -u wolfram -f"
echo "  Check status:   sudo systemctl status wolfram"
echo "  Stop:           sudo systemctl stop wolfram"
echo "  Uninstall:      sudo ./uninstall.sh"
echo ""
