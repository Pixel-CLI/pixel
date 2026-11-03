#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
# usage: env.sh HOMEDIR <binary> <args...>: the isolated HOME/XDG a session of the bench uses
H=$1; shift
mkdir -p "$H"
export HOME=$H XDG_CONFIG_HOME=$H/.config XDG_DATA_HOME=$H/.local/share XDG_STATE_HOME=$H/.local/state XDG_CACHE_HOME=$H/.cache
export PIXEL_NO_UPDATE_CHECK=1 PIXEL_MVP_TELEMETRY=0
unset RIPGREP_CONFIG_PATH GREP_OPTIONS
exec "$@"
