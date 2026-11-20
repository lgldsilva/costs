#!/bin/bash
# Backup diario do store.sqlite (LaunchAgent). Copia local com retencao de 30 dias
# and ships it to $COSTS_BACKUP_REMOTE when reachable.
. "$(dirname "${BASH_SOURCE[0]}")/costs-env.sh"
# Instancia unica: o proprio `costs backup` segura backup.lock.
REMOTE_ARGS=()
if ssh -o BatchMode=yes -o ConnectTimeout=5 "${COSTS_BACKUP_REMOTE:-backup-server}" true 2>/dev/null; then
  REMOTE_ARGS=(--remote "${COSTS_BACKUP_REMOTE:-backup-server}")
else
  echo "backup remote unreachable: local backup only"
fi
"$(costs_bin)" backup --keep 30 "${REMOTE_ARGS[@]}"
