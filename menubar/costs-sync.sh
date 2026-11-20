#!/bin/bash
# Coleta de hora em hora (LaunchAgent). O plugin costs.5m.sh so le o banco.
. "$(dirname "${BASH_SOURCE[0]}")/costs-env.sh"
# Instancia unica: o proprio `costs sync` segura sync.lock (lock do kernel).
exec "$(costs_bin)" sync
