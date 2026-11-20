# Funcoes comuns dos scripts de automacao (sourced; nao executar).

# Ambiente do launchd/SwiftBar: PATH minimo para o costs e o que ele chama (ssh, grok).
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/bin:/bin:${PATH:-}"

# Binario: $COSTS_BIN, senao ~/.local/bin/costs, senao target/release/costs do repo.
costs_bin() {
  if [ -n "${COSTS_BIN:-}" ]; then
    printf '%s\n' "$COSTS_BIN"
  elif [ -x "$HOME/.local/bin/costs" ]; then
    printf '%s\n' "$HOME/.local/bin/costs"
  else
    printf '%s\n' "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/release/costs"
  fi
}
