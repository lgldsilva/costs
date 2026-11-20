#!/bin/bash
# <xbar.title>costs</xbar.title>
# <xbar.desc>Gasto do mes e quota/saldo dos provedores (via costs menubar)</xbar.desc>
# Plugin do SwiftBar. Instalar: ln -sf ~/Projetos/costs/menubar/costs.5m.sh \
#   "$HOME/Library/Application Support/SwiftBar/Plugins/costs.5m.sh"
# O nome "5m" define o intervalo de atualizacao automatica; o item "Atualizar quota" forca.
# SwiftBar segue symlinks: resolve o diretorio real do script para achar costs-env.sh.
SELF="${BASH_SOURCE[0]}"
[ -L "$SELF" ] && SELF="$(readlink "$SELF")"
. "$(cd "$(dirname "$SELF")" && pwd)/costs-env.sh"
BIN="$(costs_bin)"
# Refresh short-lived session windows before the read-only menu snapshot, but only
# what is past the cache TTL; "Atualizar saldos" in the menu forces it.
# Provider failures preserve last good data in the cache and remain visible.
if ! "$BIN" quota --if-stale >/dev/null; then
  echo "AVISO: quota refresh failed; displaying cached data" >&2
fi
exec "$BIN" menubar
