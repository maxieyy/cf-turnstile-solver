#!/usr/bin/env bash
# =============================================================================
#
#      ███████╗ ██████╗ ██╗    ██╗███████╗██████╗ ██████╗
#      ██╔════╝██╔═══██╗██║    ██║██╔════╝██╔══██╗██╔══██╗
#      ███████╗██║   ██║██║ █╗ ██║█████╗  ██████╔╝██████╔╝
#      ╚════██║██║   ██║██║███╗██║██╔══╝  ██╔══██╗██╔══██╗
#      ███████║╚██████╔╝╚███╔███╔╝███████╗██║  ██║██████╔╝
#      ╚══════╝ ╚═════╝  ╚══╝╚══╝ ╚══════╝╚═╝  ╚═╝╚═════╝
#
#   one-command installer: rust build, chrome, systemd, optional sqlite,
#   optional caddy TLS. the api itself always binds 127.0.0.1; caddy is the
#   only thing that faces the internet.
#
#   quick run:
#     curl -fsSL https://raw.githubusercontent.com/maxieyy/cf-turnstile-solver/main/install.sh | bash
#   non-interactive:
#     ... | bash -s -- --domain ip.example.com --db yes --token s3cret
# =============================================================================
set -euo pipefail

# ---------------------------------------------------------------- appearance
if [[ -t 1 ]]; then
  C_RESET=$'\033[0m'; C_BOLD=$'\033[1m'; C_DIM=$'\033[2m'
  C_RED=$'\033[31m'; C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'
  C_BLUE=$'\033[34m'; C_MAGENTA=$'\033[35m'; C_CYAN=$'\033[36m'
  C_WHITE=$'\033[97m'; C_GREY=$'\033[90m'
else
  C_RESET=""; C_BOLD=""; C_DIM=""; C_RED=""; C_GREEN=""; C_YELLOW=""
  C_BLUE=""; C_MAGENTA=""; C_CYAN=""; C_WHITE=""; C_GREY=""
fi

readonly INSTALL_DIR="/opt/solver"
readonly CONFIG_DIR="/etc/solver"
readonly DATA_DIR="/var/lib/solver"
readonly SERVICE_NAME="solver"
readonly REPO_URL="${REPO_URL:-https://github.com/maxieyy/cf-turnstile-solver}"
readonly INTERNAL_PORT="${INTERNAL_PORT:-8907}"

banner() {
  echo -e "${C_MAGENTA}"
  cat <<'ART'
     ███████╗ ██████╗ ██╗    ██╗███████╗██████╗ ██████╗
     ██╔════╝██╔═══██╗██║    ██║██╔════╝██╔══██╗██╔══██╗
     ███████╗██║   ██║██║ █╗ ██║█████╗  ██████╔╝██████╔╝
     ╚════██║██║   ██║██║███╗██║██╔══╝  ██╔══██╗██╔══██╗
     ███████║╚██████╔╝╚███╔███╔╝███████╗██║  ██║██████╔╝
     ╚══════╝ ╚═════╝  ╚══╝╚══╝ ╚══════╝╚═╝  ╚═╝╚═════╝
ART
  echo -e "${C_RESET}${C_BOLD}   cloudflare solver + ip intelligence api${C_RESET}${C_GREY}  ·  zero deps  ·  one binary  ·  caddy tls${C_RESET}"
  echo ""
}

hr()      { echo -e "${C_GREY}  ─────────────────────────────────────────────────────────${C_RESET}"; }
step()    { echo -e "  ${C_CYAN}▸${C_RESET} ${C_BOLD}$*${C_RESET}"; }
ok()      { echo -e "  ${C_GREEN}✔${C_RESET} $*"; }
warn()    { echo -e "  ${C_YELLOW}●${C_RESET} $*"; }
fail()    { echo -e "  ${C_RED}✘ $*${C_RESET}"; exit 1; }
ask()     { echo -e "  ${C_MAGENTA}?${C_RESET} ${C_BOLD}$*${C_RESET}"; }

# spinner: run a command, show a live braille spinner + label
SPIN_PID=""
spin_start() {
  local label="$1"
  (
    local chars="⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏" i=0
    while true; do
      printf "\r  ${C_CYAN}%s${C_RESET} ${C_DIM}%s${C_RESET}" "${chars:i%10:1}" "$label"
      i=$((i+1)); sleep 0.1
    done
  ) & SPIN_PID=$!
}
spin_end() {
  kill "$SPIN_PID" 2>/dev/null || true; wait "$SPIN_PID" 2>/dev/null || true
  printf "\r\033[K"
}
run() { # run <label> <cmd...>
  local label="$1"; shift
  spin_start "$label"
  if "$@" >/tmp/solver-install.log 2>&1; then
    spin_end; ok "$label"
  else
    spin_end; fail "$label failed — tail of /tmp/solver-install.log:"
    tail -20 /tmp/solver-install.log | sed 's/^/      /' ; exit 1
  fi
}

prompt_value() { # prompt_value <var> <question> <default>
  local __var="$1" __q="$2" __def="${3:-}" __in=""
  ask "$__q ${C_GREY}[${__def:-none}]${C_RESET}"
  read -r -p "  > " __in </dev/tty || __in=""
  printf -v "$__var" '%s' "${__in:-$__def}"
}
prompt_yes() { # prompt_yes <var> <question> <default y|n>
  local __var="$1" __q="$2" __def="${3:-y}" __in=""
  ask "$__q ${C_GREY}[${__def}]${C_RESET}"
  read -r -p "  > " __in </dev/tty || __in=""
  __in="${__in:-$__def}"
  [[ "$__in" =~ ^[Yy] ]] && printf -v "$__var" "yes" || printf -v "$__var" "no"
}

panel() { # panel <title> <line...>
  echo -e "${C_BLUE}  ╭─────────────────────────────────────────────────────────╮${C_RESET}"
  echo -e "${C_BLUE}  │${C_RESET} ${C_BOLD}$1${C_RESET}"
  shift; local line
  for line in "$@"; do
    echo -e "${C_BLUE}  │${C_RESET}  $line"
  done
  echo -e "${C_BLUE}  ╰─────────────────────────────────────────────────────────╯${C_RESET}"
}

# ---------------------------------------------------------------- basics
[[ $EUID -eq 0 ]] || exec sudo -E bash "$0" "$@"

detect_pkg() {
  for m in apt-get dnf yum zypper; do
    command -v "$m" >/dev/null 2>&1 && { PKG="$m"; return; }
  done
  fail "no supported package manager found (apt/dnf/yum/zypper)"
}

ram_mib() { awk '/MemTotal/{print int($2/1024)}' /proc/meminfo; }
has_swap() { [[ $(awk '/SwapTotal/{print $2}' /proc/meminfo) -gt 0 ]]; }

ensure_swap() { # rust linking loves memory; small boxes need a little help
  if (( $(ram_mib) < 1600 )) && ! has_swap; then
    local want="no"
    prompt_yes want "Only $(ram_mib) MiB RAM and no swap — add a 2G swapfile for the build?" "y"
    [[ "$want" == "yes" ]] || return
    step "creating 2G swapfile"
    fallocate -l 2G /swapfile && chmod 600 /swapfile && mkswap /swapfile >/dev/null && swapon /swapfile
    grep -q '^/swapfile' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
    ok "swap enabled"
  fi
}

install_deps() {
  detect_pkg
  step "installing system dependencies (${PKG})"
  case "$PKG" in
    apt-get)
      export DEBIAN_FRONTEND=noninteractive
      apt-get update -qq
      apt-get install -y -qq curl git ca-certificates build-essential pkg-config unzip >/dev/null
      ;;
    dnf)  dnf install -y -q curl git ca-certificates gcc gcc-c++ make pkgconf-pkg-config unzip >/dev/null ;;
    yum)  yum install -y -q curl git ca-certificates gcc gcc-c++ make pkgconfig unzip >/dev/null ;;
    zypper) zypper --non-interactive install curl git ca-certificates gcc gcc-c++ make unzip >/dev/null ;;
  esac
  ok "system dependencies"
}

install_rust() {
  if command -v cargo >/dev/null 2>&1; then ok "rust toolchain already present ($(cargo --version))"; return; fi
  run "installing rust toolchain (1.80, minimal)" bash -c "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.80.0 --profile minimal >/dev/null"
  export PATH="/root/.cargo/bin:${PATH}"
}

install_chrome() {
  [[ -x "${INSTALL_DIR}/chrome/chrome" ]] && { ok "chrome already installed"; return; }
  step "fetching chrome-for-testing (~170 MB)"
  local url
  url=$(curl -fsSL --retry 3 https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json \
        | grep -o 'https://storage.googleapis.com/chrome-for-testing-public/[^"]*linux64/chrome-linux64.zip' | head -n1)
  [[ -n "$url" ]] || fail "could not resolve chrome download url"
  mkdir -p "${INSTALL_DIR}"
  spin_start "downloading + unpacking chrome"
  curl -fL --retry 3 -o /tmp/chrome.zip "$url" \
    && unzip -q -o /tmp/chrome.zip -d "${INSTALL_DIR}" \
    && mv "${INSTALL_DIR}/chrome-linux64" "${INSTALL_DIR}/chrome" \
    && rm -f /tmp/chrome.zip
  spin_end; ok "chrome installed at ${INSTALL_DIR}/chrome"
  detect_pkg
  case "$PKG" in
    apt-get) apt-get install -y -qq fonts-liberation libnss3 libnspr4 libdbus-1-3 libatk1.0-0 libatk-bridge2.0-0 libcups2 libdrm2 libxcomposite1 libxdamage1 libxfixes3 libxrandr2 libgbm1 libpango-1.0-0 libcairo2 libasound2 libxshmfence1 libx11-xcb1 libxkbcommon0 >/dev/null 2>&1 || true ;;
    dnf|yum) "$PKG" install -y -q nss mesa-libgbm alsa-lib libXScrnSaver >/dev/null 2>&1 || true ;;
    zypper) zypper --non-interactive install mozilla-nss libgbm1 alsa libX11-xcb1 >/dev/null 2>&1 || true ;;
  esac
  ok "chrome runtime libraries"
}

fetch_source() {
  if [[ -f "${INSTALL_DIR}/src/Cargo.toml" ]]; then
    step "source already at ${INSTALL_DIR}/src — pulling updates"
    git -C "${INSTALL_DIR}/src" pull --ff-only >/dev/null 2>&1 && ok "source updated" || warn "git pull failed, keeping existing source"
  else
    run "cloning ${REPO_URL}" git clone --depth 1 "$REPO_URL" "${INSTALL_DIR}/src"
  fi
}

build_binary() {
  local features="$1"
  export PATH="/root/.cargo/bin:${PATH}"
  step "compiling release binary ${C_GREY}(2-6 min on a small vcpu; one time only)${C_RESET}"
  spin_start "cargo build --release ${features:+--features $features}"
  (
    cd "${INSTALL_DIR}/src"
    cargo build --release --quiet ${features:+--features "$features"}
  ) >/tmp/solver-install.log 2>&1
  spin_end
  [[ -x "${INSTALL_DIR}/src/target/release/solver" ]] || { fail "build failed"; tail -30 /tmp/solver-install.log; }
  mkdir -p "${INSTALL_DIR}/bin"
  cp "${INSTALL_DIR}/src/target/release/solver" "${INSTALL_DIR}/bin/solver"
  ok "binary ready: ${INSTALL_DIR}/bin/solver"
}

write_config() { # write_config <db_yes> <token>
  mkdir -p "$CONFIG_DIR"
  local db_yes="$1" token="${2:-}"
  local db_field="null"
  [[ "$db_yes" == "yes" ]] && db_field="\"${DATA_DIR}/solver.db\""
  local token_field="null"
  [[ -n "$token" ]] && token_field="\"${token}\""
  cat > "${CONFIG_DIR}/config.json" <<EOF
{
  "host": "127.0.0.1",
  "port": ${INTERNAL_PORT},
  "timeout_ms": 29000,
  "headless": true,
  "prewarm": true,
  "api_token": ${token_field},
  "db_path": ${db_field},
  "rate_limit": { "enabled": true, "capacity": 10, "refill_per_sec": 0.5 },
  "cache": { "enabled": true, "ttl_secs": 900, "max_entries": 2048, "stale_grace_secs": 3600 }
}
EOF
  # browsers/tabs intentionally omitted: the binary auto-sizes from RAM + cores
  ok "config written: ${CONFIG_DIR}/config.json (browsers/tabs auto-sized)"
}

write_systemd() {
  cat > "/etc/systemd/system/${SERVICE_NAME}.service" <<EOF
[Unit]
Description=Cloudflare solver + IP intelligence API
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=${INSTALL_DIR}
ExecStart=${INSTALL_DIR}/bin/solver
Environment=CHROME_BIN=${INSTALL_DIR}/chrome/chrome
Environment=CONFIG=${CONFIG_DIR}/config.json
Environment=DYNO=\${HOSTNAME}
Restart=always
RestartSec=3
LimitNOFILE=65535
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=${DATA_DIR} ${CONFIG_DIR} /tmp

[Install]
WantedBy=multi-user.target
EOF
  systemctl daemon-reload
  systemctl enable "${SERVICE_NAME}" >/dev/null 2>&1
  systemctl restart "${SERVICE_NAME}"
  ok "systemd service enabled (${SERVICE_NAME})"
}

wait_health() {
  for _ in $(seq 1 30); do
    curl -sf -m 2 "http://127.0.0.1:${INTERNAL_PORT}/health" >/dev/null 2>&1 && { ok "health check passed"; return; }
    sleep 1
  done
  warn "health check not responding yet — check: journalctl -u ${SERVICE_NAME} -n 50"
}

setup_caddy() { # setup_caddy <domain|ip-only>
  local domain="$1"
  detect_pkg
  step "installing caddy"
  case "$PKG" in
    apt-get)
      apt-get install -y -qq debian-keyring debian-archive-keyring apt-transport-https curl >/dev/null
      install -d /usr/share/keyrings
      curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --batch --yes --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg 2>/dev/null
      curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
      apt-get update -qq && apt-get install -y -qq caddy >/dev/null
      ;;
    dnf|yum) "$PKG" install -y -q caddy >/dev/null || fail "install caddy manually for $PKG" ;;
    zypper) zypper --non-interactive install caddy >/dev/null || fail "install caddy manually for zypper" ;;
  esac
  ok "caddy installed"

  if [[ "$domain" == "IP_ONLY" ]]; then
    cat > /etc/caddy/Caddyfile <<EOF
# plain http on :80 -> api (no domain to issue TLS for yet)
:80 {
  reverse_proxy 127.0.0.1:${INTERNAL_PORT}
}
EOF
    warn "no domain given — serving plain http://<vps-ip>/ (run option 2 later to add TLS)"
  else
    cat > /etc/caddy/Caddyfile <<EOF
# managed by solver installer — auto-HTTPS via Let's Encrypt
${domain} {
  reverse_proxy 127.0.0.1:${INTERNAL_PORT}
}
EOF
  fi
  systemctl enable --now caddy >/dev/null 2>&1
  systemctl reload caddy >/dev/null 2>&1 || systemctl restart caddy
  ok "caddy reverse proxying :443/${domain} -> 127.0.0.1:${INTERNAL_PORT}"
}

final_panel() {
  local domain="$1" token="${2:-}"
  local base="https://${domain}"
  [[ "$domain" == "IP_ONLY" || -z "$domain" ]] && base="http://$(curl -fsS -m 3 ifconfig.me 2>/dev/null || echo VPS_IP)"
  local auth=""
  [[ -n "$token" ]] && auth=$'\n     -H "Authorization: Bearer '"$token"'"'
  echo ""
  panel "🎉 deployment complete" "" \
    "endpoints:" \
    "  POST ${base}/v1/solver   {\"url\":\"https://site.com\"}" \
    "  POST ${base}/v1/ip       {\"ip\":\"1.2.3.4\"}" \
    "  GET  ${base}/health" "" \
    "try it:" \
    "  curl -X POST ${base}/v1/ip \\$auth" \
    "       -H 'content-type: application/json' \\" \
    "       -d '{\"ip\":\"197.157.165.49\"}'" "" \
    "manage:" \
    "  systemctl status ${SERVICE_NAME}   ·  journalctl -u ${SERVICE_NAME} -f" \
    "  re-run: curl -fsSL ${REPO_URL}/raw/main/install.sh | bash"
  echo ""
}

# ---------------------------------------------------------------- actions
action_install() {
  local domain="" db="ask" token="" noninteractive="no"
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --domain) domain="$2"; shift 2 ;;
      --db)     db="$2"; shift 2 ;;
      --token)  token="$2"; shift 2 ;;
      *) shift ;;
    esac
  done
  [[ -n "$domain" || -n "${DB:-}" ]] && noninteractive="yes"

  banner
  hr
  step "fresh install on $(hostname) — $(ram_mib) MiB RAM, $(nproc) vCPU"
  hr

  if [[ "$db" == "ask" ]]; then
    local dbq="no"
    prompt_yes dbq "Persist requests + cache to sqlite (survives restarts)?" "y"
    db="$dbq"
  fi
  if [[ -z "$domain" ]]; then
    prompt_value domain "TLS domain (must already point at this VPS's A/AAAA record) — blank to skip for now"
  fi

  ensure_swap
  install_deps
  install_rust
  install_chrome
  fetch_source
  mkdir -p "$DATA_DIR"
  local features=""
  [[ "$db" == "yes" ]] && features="db"
  build_binary "$features"
  write_config "$db" "$token"
  write_systemd
  wait_health
  if [[ -n "$domain" ]]; then
    setup_caddy "$domain"
  else
    setup_caddy "IP_ONLY"
  fi
  final_panel "$domain" "$token"
}

action_tls() {
  banner
  local domain=""
  prompt_value domain "Domain for TLS (A/AAAA must point here)"
  [[ -z "$domain" ]] && { warn "no domain — keeping current config"; return; }
  setup_caddy "$domain"
  panel "🔐 TLS live" \
    "https://${domain}/health" \
    "certificate: auto-issued + renewed by caddy (Let's Encrypt)"
}

action_config() {
  banner
  local editor="${EDITOR:-vi}"
  warn "opening ${CONFIG_DIR}/config.json in ${editor}"
  "${editor}" "${CONFIG_DIR}/config.json" </dev/tty >/dev/tty 2>&1 || true
  systemctl restart "${SERVICE_NAME}" && ok "service restarted with new config"
  wait_health
  curl -sf "http://127.0.0.1:${INTERNAL_PORT}/config" | sed 's/^/    /'
  echo ""
}

action_status() {
  banner
  systemctl --no-pager -l status "${SERVICE_NAME}" | head -12
  hr
  curl -sf -m 3 "http://127.0.0.1:${INTERNAL_PORT}/health" | python3 -m json.tool 2>/dev/null \
    || curl -sf -m 3 "http://127.0.0.1:${INTERNAL_PORT}/health" || true
  echo ""
}

action_update() {
  banner
  fetch_source
  local features=""
  grep -q '"db_path": "' "${CONFIG_DIR}/config.json" 2>/dev/null && features="db"
  build_binary "$features"
  systemctl restart "${SERVICE_NAME}"
  wait_health
  ok "update complete"
}

action_uninstall() {
  banner
  local sure="no"
  prompt_yes sure "Really remove the solver from this machine? (db is kept)" "n"
  [[ "$sure" == "yes" ]] || { warn "aborted"; return; }
  systemctl disable --now "${SERVICE_NAME}" >/dev/null 2>&1 || true
  rm -rf "${INSTALL_DIR}/bin" "${INSTALL_DIR}/chrome"
  rm -f "/etc/systemd/system/${SERVICE_NAME}.service"
  systemctl daemon-reload
  ok "solver removed (source kept at ${INSTALL_DIR}/src, db kept at ${DATA_DIR})"
}

action_menu() {
  banner
  while true; do
    echo -e "  ${C_BOLD}main menu${C_RESET}"
    hr
    echo -e "   ${C_CYAN}1${C_RESET}  fresh install            (deps · chrome · build · systemd · tls)"
    echo -e "   ${C_CYAN}2${C_RESET}  add / replace tls domain"
    echo -e "   ${C_CYAN}3${C_RESET}  edit configuration        (rate limit · cache · token · db)"
    echo -e "   ${C_CYAN}4${C_RESET}  service status + health"
    echo -e "   ${C_CYAN}5${C_RESET}  update to latest source"
    echo -e "   ${C_CYAN}6${C_RESET}  uninstall"
    echo -e "   ${C_GREY}q${C_RESET}  quit"
    hr
    local choice=""
    read -r -p "  choose › " choice </dev/tty || exit 0
    case "$choice" in
      1) action_install "$@" ;;
      2) action_tls ;;
      3) action_config ;;
      4) action_status ;;
      5) action_update ;;
      6) action_uninstall ;;
      q|Q) echo -e "  ${C_GREY}bye 👋${C_RESET}"; exit 0 ;;
      *) warn "pick 1-6 or q" ;;
    esac
  done
}

# ---------------------------------------------------------------- entry
case "${1:-menu}" in
  install|fresh) shift; action_install "$@" ;;
  tls)           shift; action_tls ;;
  config)        shift; action_config ;;
  status)        shift; action_status ;;
  update)        shift; action_update ;;
  uninstall)     shift; action_uninstall ;;
  menu|"")       action_menu "$@" ;;
  *)             action_menu ;;
esac
