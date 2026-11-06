#!/bin/sh
set -e

# Wolfram uninstaller — stops service, removes binary, service file, and optionally config/data.

INSTALL_BIN="/usr/local/bin/wolfram"
SERVICE_FILE="/etc/systemd/system/wolfram.service"
CONFIG_DIR="/etc/wolfram"
DATA_DIR="/var/lib/wolfram"

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

if [ "$(id -u)" -ne 0 ]; then
    error "This script must be run as root (sudo ./uninstall.sh)"
fi

# Stop and disable service.
if systemctl is-active --quiet wolfram 2>/dev/null; then
    info "Stopping wolfram service..."
    systemctl stop wolfram
fi

if systemctl is-enabled --quiet wolfram 2>/dev/null; then
    info "Disabling wolfram service..."
    systemctl disable wolfram
fi

# Put back the kernel values Wolfram changed (normally already done by the
# unit's ExecStopPost; this covers older units and a stopped-but-unrestored
# state).
if [ -x "$INSTALL_BIN" ]; then
    info "Restoring kernel tuning changed by wolfram..."
    "$INSTALL_BIN" --restore-tuning || warn "Could not restore kernel tuning — check $DATA_DIR/original-tuning.json"
fi

# Remove service file.
if [ -f "$SERVICE_FILE" ]; then
    info "Removing $SERVICE_FILE"
    rm -f "$SERVICE_FILE"
    systemctl daemon-reload
fi

# Remove binary.
if [ -f "$INSTALL_BIN" ]; then
    info "Removing $INSTALL_BIN"
    rm -f "$INSTALL_BIN"
fi

# Ask about config and data.
echo ""
if [ -d "$CONFIG_DIR" ]; then
    printf "Remove config directory %s? [y/N] " "$CONFIG_DIR"
    read -r answer
    case "$answer" in
        [yY]*)
            info "Removing $CONFIG_DIR"
            rm -rf "$CONFIG_DIR"
            ;;
        *)
            warn "Keeping $CONFIG_DIR"
            ;;
    esac
fi

if [ -d "$DATA_DIR" ]; then
    printf "Remove data directory %s (includes checkpoints)? [y/N] " "$DATA_DIR"
    read -r answer
    case "$answer" in
        [yY]*)
            info "Removing $DATA_DIR"
            rm -rf "$DATA_DIR"
            ;;
        *)
            warn "Keeping $DATA_DIR"
            ;;
    esac
fi

echo ""
info "Wolfram uninstalled."
