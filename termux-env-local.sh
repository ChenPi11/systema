#!/bin/sh
# ---------------------------------------------------------------------------
# termux-env.sh — Build-time path configuration for Termux (Android).
#
# Usage:
#   source termux-env.sh
#   cargo build
#
# Termux uses a non-standard filesystem layout rooted at $LOCAL_PREFIX
# (typically /data/data/com.termux/files/usr).  This script sets the
# environment variables that control the install paths compiled into
# system-a and system-s so that they match the Termux layout.
#
# See crates/common/build.rs for the full list of recognised variables.
# ---------------------------------------------------------------------------

set -u

# $PREFIX is normally set by Termux itself; fall back to the default.
: "${PREFIX:=/data/data/com.termux/files/usr}"
export LOCAL_PREFIX="${PREFIX}/local"

# ---------------------------------------------------------------------------
# system-alphabet directories
# ---------------------------------------------------------------------------
export SYSTEMA_ETC_DIR="${LOCAL_PREFIX}/etc/system-alphabet"
export SYSTEMA_RUN_DIR="${PREFIX}/var/run/system-alphabet"
export SYSTEMA_LOCAL_LIB_DIR="${LOCAL_PREFIX}/lib/system-alphabet"
export SYSTEMA_LIB_DIR="${LOCAL_PREFIX}/lib/system-alphabet"

# ---------------------------------------------------------------------------
# systemd directories (used to read real systemd unit files when present)
# ---------------------------------------------------------------------------
export SYSTEMD_ETC_UNIT_DIR="${LOCAL_PREFIX}/etc/systemd/system"
export SYSTEMD_LIB_UNIT_DIR="${LOCAL_PREFIX}/lib/systemd/system"
export SYSTEMD_ALT_UNIT_DIR="${LOCAL_PREFIX}/lib/systemd/system"

# ---------------------------------------------------------------------------
# systemd generator directories
# ---------------------------------------------------------------------------
export SYSTEMD_GENERATOR_RUN_DIR="${PREFIX}/var/run/systemd/generator"
export SYSTEMD_GENERATOR_LATE_DIR="${PREFIX}/var/run/systemd/generator.late"
export SYSTEMD_GENERATOR_ETC_DIR="${LOCAL_PREFIX}/etc/systemd/system-generators"
export SYSTEMD_GENERATOR_LOCAL_DIR="${LOCAL_PREFIX}/lib/systemd/system-generators"
export SYSTEMD_GENERATOR_LIB_DIR="${LOCAL_PREFIX}/lib/systemd/system-generators"
export SYSTEMD_GENERATOR_ALT_DIR="${LOCAL_PREFIX}/lib/systemd/system-generators"

# ---------------------------------------------------------------------------
# Miscellaneous system paths
# ---------------------------------------------------------------------------
export SYSTEMD_FIRST_BOOT_FILE="${PREFIX}/var/run/systemd/first-boot"
export SYSTEMD_MACHINE_ID_FILE="${LOCAL_PREFIX}/etc/machine-id"

# ---------------------------------------------------------------------------
# IPC socket
# ---------------------------------------------------------------------------
export SYSTEMA_IPC_SOCKET="${PREFIX}/var/run/system-alphabet/allocator.sock"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo "termux-env: LOCAL_PREFIX = ${LOCAL_PREFIX}"
echo "termux-env: All SYSTEMA_* / SYSTEMD_* variables exported."
echo "termux-env: Ready. Run 'cargo build'."
