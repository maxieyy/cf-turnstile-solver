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
#   fully idempotent: safe to re-run any time, it converges to a good state.
#
#   quick run (as root):
#     curl -fsSL https://raw.githubusercontent.com/maxieyy/cf-turnstile-solver/main/install.sh | bash
#   non-interactive:
#     curl -fsSL ... | bash -s -- install --domain ip.example.com --db yes --token s3cret
# =============================================================================
set -euo pipefail

SELF_URL="${REPO_URL:-https://github.com/maxieyy/cf-turnstile-solver}/raw/main/install.sh"

# ---------------------------------------------------------------- elevate
# must run as root; handle both file execution and `curl | bash`
if [[ $EUID -ne 0 ]]; then
  if [[ -s "$0" && -f "$0" && "$0" != "bash" && "$0" != "/dev/stdin" ]]; then
    exec sudo -E bash "$0" "$@"
  fi
  # piped: re-download as root (stdin is already consumed by this shell)
  exec sudo -E bash -c "$(curl -fsSL "$SELF_URL")" solver-install "$@"
fi

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
detect_pkg() {
  for m in apt-get dnf yum zypper; do
    command -v "$m" >/dev/null 2>&1 && { PKG="$m"; return; }
  done
  fail "no supported package manager found (apt/dnf/yum/zypper)"
}

ram_mib() { awk '/MemTotal/{print int($2/1024)}' /proc/meminfo; }
has_swap() { [[ $(awk '/SwapTotal/{print $2}' /proc/meminfo) -gt 0 ]]; }

ensure_swap() {
  if (( $(ram_mib) < 1600 )) && ! has_swap && [[ ! -f /swapfile ]]; then
    local want="no"
    prompt_yes want "Only $(ram_mib) MiB RAM and no swap — add a 2G swapfile for the build?" "y"
    [[ "$want" == "yes" ]] || return
    step "creating 2G swapfile"
    fallocate -l 2G /swapfile && chmod 600 /swapfile && mkswap /swapfile >/dev/null && swapon /swapfile
    grep -q '^/swapfile' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
    ok "swap enabled"
  elif [[ -f /swapfile ]] && ! has_swap; then
    swapon /swapfile 2>/dev/null && ok "swap re-enabled" || true
  else
    ok "memory ok ($(ram_mib) MiB RAM, swap: $(has_swap && echo yes || echo no))"
  fi
}

pkg_install() { # pkg_install <pkgs...>
  detect_pkg
  case "$PKG" in
    apt-get) DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" >/dev/null ;;
    dnf)     dnf install -y -q "$@" >/dev/null ;;
    yum)     yum install -y -q "$@" >/dev/null ;;
    zypper)  zypper --non-interactive install "$@" >/dev/null ;;
  esac
}

install_deps() {
  detect_pkg
  step "installing system dependencies (${PKG})"
  case "$PKG" in
    apt-get)
      export DEBIAN_FRONTEND=noninteractive
      apt-get update -qq >/dev/null 2>&1 || warn "apt update had warnings, continuing"
      pkg_install curl git ca-certificates build-essential pkg-config unzip python3
      ;;
    dnf)  pkg_install curl git ca-certificates gcc gcc-c++ make pkgconf-pkg-config unzip python3 ;;
    yum)  pkg_install curl git ca-certificates gcc gcc-c++ make pkgconfig unzip python3 ;;
    zypper) pkg_install curl git ca-certificates gcc gcc-c++ make unzip python3 ;;
  esac
  ok "system dependencies"
}

install_rust() {
  if command -v cargo >/dev/null 2>&1 || [[ -x /root/.cargo/bin/cargo ]]; then
    export PATH="/root/.cargo/bin:${PATH}"
    ok "rust toolchain already present ($(cargo --version 2>/dev/null || echo unknown))"
    return
  fi
  run "installing rust toolchain (1.80, minimal)" bash -c "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.80.0 --profile minimal >/dev/null"
  export PATH="/root/.cargo/bin:${PATH}"
}

install_chrome() {
  if [[ -x "${INSTALL_DIR}/chrome/chrome" ]]; then ok "chrome already installed"; return; fi
  step "fetching chrome-for-testing (~170 MB)"
  local url
  url=$(curl -fsSL --retry 3 https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json \
        | grep -o 'https://storage.googleapis.com/chrome-for-testing-public/[^"]*linux64/chrome-linux64.zip' | head -n1)
  [[ -n "$url" ]] || fail "could not resolve chrome download url"
  mkdir -p "${INSTALL_DIR}"
  spin_start "downloading + unpacking chrome"
  curl -fsSL --retry 3 -o /tmp/chrome.zip "$url" >/dev/null \
    && unzip -q -o /tmp/chrome.zip -d "${INSTALL_DIR}" \
    && mv "${INSTALL_DIR}/chrome-linux64" "${INSTALL_DIR}/chrome" \
    && rm -f /tmp/chrome.zip
  spin_end; ok "chrome installed at ${INSTALL_DIR}/chrome"
  detect_pkg
  case "$PKG" in
    apt-get) pkg_install fonts-liberation libnss3 libnspr4 libdbus-1-3 libatk1.0-0 libatk-bridge2.0-0 libcups2 libdrm2 libxcomposite1 libxdamage1 libxfixes3 libxrandr2 libgbm1 libpango-1.0-0 libcairo2 libasound2 libxshmfence1 libx11-xcb1 libxkbcommon0 2>/dev/null || true ;;
    dnf|yum) pkg_install nss mesa-libgbm alsa-lib libXScrnSaver 2>/dev/null || true ;;
    zypper)  pkg_install mozilla-nss libgbm1 alsa libX11-xcb1 2>/dev/null || true ;;
  esac
  ok "chrome runtime libraries"
}

fetch_source() {
  if [[ -d "${INSTALL_DIR}/src/.git" ]]; then
    step "source exists at ${INSTALL_DIR}/src — pulling latest"
    if git -C "${INSTALL_DIR}/src" fetch origin main >/dev/null 2>&1 \
       && git -C "${INSTALL_DIR}/src" reset --hard origin/main >/dev/null 2>&1; then
      ok "source at latest ($(git -C "${INSTALL_DIR}/src" log --oneline -1 | cut -c1-8))"
    else
      warn "git fetch failed, keeping existing source"
    fi
  else
    rm -rf "${INSTALL_DIR}/src"
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

# read a scalar out of /etc/solver/config.json (python3 when available)
cfg_get() { # cfg_get <json-path-like-grep>
  local key="$1"
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$CONFIG_DIR/config.json" "$key" <<'PY' 2>/dev/null
import json, sys
try:
    with open(sys.argv[1]) as f: cfg = json.load(f)
    key = sys.argv[2]
    if key == "api_token":
        v = cfg.get("api_token")
        print(v if isinstance(v, str) else "")
    elif key == "db_path":
        v = cfg.get("db_path")
        print("db" if v else "nodb")
    elif key == "port":
        print(cfg.get("port", 8907))
except Exception:
    pass
PY
  else
    case "$key" in
      api_token) grep -o '"api_token"[^,}]*' "$CONFIG_DIR/config.json" 2>/dev/null | grep -o '"[^"]*"$' | tr -d '"' || true ;;
      db_path)   grep -q '"db_path": "' "$CONFIG_DIR/config.json" 2>/dev/null && echo db || echo nodb ;;
      port)      grep -o '"port": [0-9]*' "$CONFIG_DIR/config.json" 2>/dev/null | grep -o '[0-9]*' || echo "$INTERNAL_PORT" ;;
    esac
  fi
}

write_config() { # write_config <db_yes> <token> <force>
  mkdir -p "$CONFIG_DIR"
  if [[ "$3" != "force" && -s "${CONFIG_DIR}/config.json" ]]; then
    ok "existing config kept: ${CONFIG_DIR}/config.json"
    return
  fi
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
  local port="${1:-$(cfg_get port)}"; port="${port:-$INTERNAL_PORT}"
  for _ in $(seq 1 30); do
    curl -sf -m 2 "http://127.0.0.1:${port}/health" >/dev/null 2>&1 && { ok "health check passed"; return 0; }
    sleep 1
  done
  warn "health check not responding — check: journalctl -u ${SERVICE_NAME} -n 50"
  return 1
}

# ---------------------------------------------------------------- caddy
caddy_static_install() {
  local arch
  arch=$(uname -m)
  case "$arch" in
    x86_64) arch="amd64" ;;
    aarch64) arch="arm64" ;;
    armv7l) arch="armv7" ;;
  esac
  step "installing caddy (static binary, linux/${arch})"
  spin_start "downloading caddy"
  curl -fsSL --retry 3 "https://caddyserver.com/api/download?os=linux&arch=${arch}" \
    -o /usr/local/bin/caddy
  chmod +x /usr/local/bin/caddy
  spin_end
  getent group caddy >/dev/null || groupadd --system caddy
  id -u caddy >/dev/null 2>&1 || useradd --system --gid caddy --home /var/lib/caddy --shell /usr/sbin/nologin caddy
  mkdir -p /var/lib/caddy /etc/caddy
  setcap 'cap_net_bind_service=+ep' /usr/local/bin/caddy 2>/dev/null || true
  cat > /etc/systemd/system/caddy.service <<'EOF'
[Unit]
Description=Caddy web server
After=network-online.target
Wants=network-online.target

[Service]
User=caddy
Group=caddy
ExecStart=/usr/local/bin/caddy run --environ --config /etc/caddy/Caddyfile
ExecReload=/usr/local/bin/caddy reload --config /etc/caddy/Caddyfile --force
TimeoutStopSec=5s
LimitNOFILE=1048576
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF
  systemctl daemon-reload
}

setup_caddy() { # setup_caddy <domain|IP_ONLY>
  local domain="$1"
  detect_pkg
  local caddy_bin=""
  if command -v caddy >/dev/null 2>&1; then
    caddy_bin="$(command -v caddy)"
    ok "caddy already installed (${caddy_bin})"
  else
    step "installing caddy"
    local installed="no"
    case "$PKG" in
      apt-get)
        # try the distro package first, then the caddy repo, then static
        if apt-get install -y -qq caddy >/dev/null 2>&1; then installed="yes"
        else
          install -d /usr/share/keyrings
          curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' 2>/dev/null \
            | gpg --batch --yes --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg 2>/dev/null || true
          curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' 2>/dev/null \
            | tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null || true
          if apt-get update -qq 2>/dev/null && apt-get install -y -qq caddy >/dev/null 2>&1; then installed="yes"; fi
        fi
        ;;
      dnf|yum) "$PKG" install -y -q caddy >/dev/null 2>&1 && installed="yes" ;;
      zypper)  zypper --non-interactive install caddy >/dev/null 2>&1 && installed="yes" ;;
    esac
    if [[ "$installed" == "yes" ]]; then
      ok "caddy installed via ${PKG}"
    else
      warn "package install failed — falling back to official static binary"
      caddy_static_install
      ok "caddy static binary installed"
    fi
  fi

  mkdir -p /etc/caddy   # some caddy packages don't create it

  if [[ "$domain" == "IP_ONLY" ]]; then
    cat > /etc/caddy/Caddyfile <<EOF
# managed by solver installer — plain http until a domain is added (menu option 2)
:80 {
  reverse_proxy 127.0.0.1:${INTERNAL_PORT}
}
EOF
    warn "no domain yet — serving plain http://<vps-ip>/ (add TLS later via menu option 2)"
  else
    cat > /etc/caddy/Caddyfile <<EOF
# managed by solver installer — auto-HTTPS via Let's Encrypt
${domain} {
  reverse_proxy 127.0.0.1:${INTERNAL_PORT}
}
EOF
  fi

  systemctl enable caddy >/dev/null 2>&1
  systemctl restart caddy
  ok "caddy reverse proxying :443 ${domain} -> 127.0.0.1:${INTERNAL_PORT}"
}

caddy_domain() { # first site block in the Caddyfile, or empty
  if [[ -s /etc/caddy/Caddyfile ]]; then
    awk 'NF && $0 !~ /^#/ {gsub("{","");print $1; exit}' /etc/caddy/Caddyfile 2>/dev/null
  fi
}

# ---------------------------------------------------------------- endpoint tests
api_base() { echo "http://127.0.0.1:${INTERNAL_PORT}"; }
auth_header() { # echo -H arg when a token is set
  local token; token="$(cfg_get api_token)"
  [[ -n "$token" ]] && printf 'Authorization: Bearer %s' "$token" || true
}

pretty_kv() { # pretty_kv <json-on-stdin>
  if command -v python3 >/dev/null 2>&1; then
    DIM="${C_GREY}" BRIGHT="${C_WHITE}" RESET="${C_RESET}" python3 -c '
import json, os, sys
DIM, BRIGHT, RESET = (os.environ.get(k, "") for k in ("DIM", "BRIGHT", "RESET"))
try:
    d = json.load(sys.stdin)
    width = max((len(k) for k in d), default=0)
    for k, v in d.items():
        if isinstance(v, list):
            v = ", ".join(map(str, v)) or "[]"
        print(f"  {DIM}{k:<{width}}{RESET} : {BRIGHT}{v}{RESET}")
except Exception:
    print("  (unparseable response)")
'
  else
    sed 's/^/    /'
  fi
}

test_health() {
  banner
  step "GET /health"
  curl -sf -m 10 "$(api_base)/health" -H "$(auth_header)" | pretty_kv
  hr
  step "GET /config"
  curl -sf -m 10 "$(api_base)/config" -H "$(auth_header)" | pretty_kv
  echo ""
}

test_ip() {
  banner
  local ip=""
  prompt_value ip "IP to look up" "197.157.165.49"
  step "POST /v1/ip {\"ip\":\"${ip}\"} ${C_GREY}(first hit may take ~10s: earn clearance + fetch)${C_RESET}"
  local resp
  if resp=$(curl -sf -m 90 -X POST "$(api_base)/v1/ip" -H "content-type: application/json" -H "$(auth_header)" -d "{\"ip\":\"${ip}\"}"); then
    echo "$resp" | pretty_kv
  else
    warn "request failed (exit $?) — is the service running? check option 4"
  fi
  echo ""
}

test_solver() {
  banner
  local url=""
  prompt_value url "URL to earn clearance for" "https://nowsecure.nl"
  step "POST /v1/solver {\"url\":\"${url}\"} ${C_GREY}(~3-10s)${C_RESET}"
  local resp
  if resp=$(curl -sf -m 120 -X POST "$(api_base)/v1/solver" -H "content-type: application/json" -H "$(auth_header)" -d "{\"url\":\"${url}\"}"); then
    DIM="${C_GREY}" BRIGHT="${C_WHITE}" RESET="${C_RESET}" python3 -c '
import json, os, sys
DIM, BRIGHT, RESET = (os.environ.get(k, "") for k in ("DIM", "BRIGHT", "RESET"))
try:
    d = json.load(sys.stdin)
    h = d.get("headers", {})
    cookie = h.get("Cookie", "")
    print(f"  {DIM}status  {RESET} : {BRIGHT}{d.get('status')}{RESET}")
    print(f"  {DIM}elapsed {RESET} : {BRIGHT}{d.get('elapsed')}{RESET}")
    print(f"  {DIM}egress  {RESET} : {BRIGHT}{d.get('ip')}{RESET}")
    print(f"  {DIM}ua      {RESET} : {BRIGHT}{str(h.get('User-Agent',''))[:60]}...{RESET}")
    print(f"  {DIM}cookie  {RESET} : {BRIGHT}{cookie[:44]}...{RESET}")
except Exception:
    pass
' <<<"$resp" 2>/dev/null || echo "  $resp"
  else
    warn "request failed — check journalctl -u ${SERVICE_NAME} -n 30"
  fi
  echo ""
}

test_all() {
  banner
  step "self-test: all endpoints"
  local fails=0
  curl -sf -m 10 "$(api_base)/health" -H "$(auth_header)" >/dev/null && ok "/health" || { warn "/health FAILED"; fails=$((fails+1)); }
  curl -sf -m 10 "$(api_base)/config" -H "$(auth_header)" >/dev/null && ok "/config" || { warn "/config FAILED"; fails=$((fails+1)); }
  local ip="1.1.1.1"
  curl -sf -m 90 -X POST "$(api_base)/v1/ip" -H "content-type: application/json" -H "$(auth_header)" -d "{\"ip\":\"${ip}\"}" >/dev/null \
    && ok "/v1/ip (${ip})" || { warn "/v1/ip FAILED"; fails=$((fails+1)); }
  local pub_url="http://$(curl -fsS -m 3 ifconfig.me 2>/dev/null || echo VPS_IP)/health"
  local dom; dom="$(caddy_domain)"
  [[ -n "$dom" && "$dom" != *":"* ]] && pub_url="https://${dom}/health"
  curl -ksf -m 10 "$pub_url" >/dev/null && ok "public: ${pub_url}" || warn "public check failed (${pub_url}) — normal before DNS/TLS is ready"
  hr
  if (( fails == 0 )); then panel "✅ all endpoints healthy"; else warn "${fails} endpoint(s) failing"; fi
  echo ""
}

# ---------------------------------------------------------------- actions
action_install() {
  local domain="" db="ask" token="" force_config="no"
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --domain) domain="$2"; shift 2 ;;
      --db)     db="$2"; shift 2 ;;
      --token)  token="$2"; shift 2 ;;
      *) shift ;;
    esac
  done

  banner
  hr
  if systemctl is-active --quiet "${SERVICE_NAME}" 2>/dev/null; then
    step "existing install detected — converging (idempotent re-run)"
  else
    step "fresh install on $(hostname) — $(ram_mib) MiB RAM, $(nproc) vCPU"
  fi
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
  [[ "$db" == "yes" || "$( [[ -f "${CONFIG_DIR}/config.json" ]] && cfg_get db_path )" == "db" ]] && features="db"
  build_binary "$features"
  if [[ -s "${CONFIG_DIR}/config.json" ]]; then
    local keep="y"
    prompt_yes keep "Config already exists — keep it? ${C_GREY}(no = regenerate with your answers)${C_RESET}" "y"
    [[ "$keep" == "yes" ]] || force_config="force"
  fi
  write_config "$db" "$token" "$force_config"
  write_systemd
  wait_health || true
  setup_caddy "${domain:-IP_ONLY}"
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
  cp "${CONFIG_DIR}/config.json" "${CONFIG_DIR}/config.json.bak" 2>/dev/null || true
  "${editor}" "${CONFIG_DIR}/config.json" </dev/tty >/dev/tty 2>&1 || true
  systemctl restart "${SERVICE_NAME}" && ok "service restarted (backup at config.json.bak)"
  wait_health || true
  curl -sf "http://127.0.0.1:$(cfg_get port)/config" | pretty_kv
  echo ""
}

action_status() {
  banner
  systemctl --no-pager -l status "${SERVICE_NAME}" 2>/dev/null | head -10 || warn "service not found"
  hr
  curl -sf -m 3 "http://127.0.0.1:$(cfg_get port)/health" | pretty_kv || warn "health not responding"
  echo ""
}

action_update() {
  banner
  step "updating to latest source"
  fetch_source
  local features=""
  [[ "$(cfg_get db_path)" == "db" ]] && features="db"
  build_binary "$features"
  systemctl restart "${SERVICE_NAME}"
  wait_health || true
  panel "⬆️ update complete" \
    "service: $(systemctl is-active ${SERVICE_NAME} 2>/dev/null || echo unknown)" \
    "source:  $(git -C ${INSTALL_DIR}/src log --oneline -1 2>/dev/null | cut -c1-60)"
  echo ""
}

action_uninstall() {
  banner
  local sure="no"
  prompt_yes sure "Really remove the solver from this machine? (db and config are kept)" "n"
  [[ "$sure" == "yes" ]] || { warn "aborted"; return; }
  systemctl disable --now "${SERVICE_NAME}" >/dev/null 2>&1 || true
  rm -rf "${INSTALL_DIR}/bin" "${INSTALL_DIR}/chrome"
  rm -f "/etc/systemd/system/${SERVICE_NAME}.service"
  systemctl daemon-reload
  ok "solver removed (source kept at ${INSTALL_DIR}/src, db kept at ${DATA_DIR})"
}

final_panel() {
  local domain="$1" token="${2:-}"
  local base="https://${domain}"
  [[ -z "$domain" ]] && base="http://$(curl -fsS -m 3 ifconfig.me 2>/dev/null || echo VPS_IP)"
  local auth=""
  [[ -n "$token" ]] && auth=$'\n     -H "Authorization: Bearer '"$token"'"'
  echo ""
  panel "🎉 deployment complete" "" \
    "endpoints:" \
    "  POST ${base}/v1/solver   {\"url\":\"https://site.com\"}" \
    "  POST ${base}/v1/ip       {\"ip\":\"1.2.3.4\"}" \
    "  GET  ${base}/health" "" \
    "test from here:" \
    "  re-run this installer and pick 8 (test /v1/ip)" "" \
    "manage:" \
    "  systemctl status ${SERVICE_NAME}   ·  journalctl -u ${SERVICE_NAME} -f" \
    "  re-run any time (idempotent): curl -fsSL ${REPO_URL}/raw/main/install.sh | bash"
  echo ""
}

action_menu() {
  banner
  while true; do
    echo -e "  ${C_BOLD}main menu${C_RESET} ${C_GREY}· $(hostname) · $(ram_mib) MiB · service: $(systemctl is-active ${SERVICE_NAME} 2>/dev/null || echo not-installed)${C_RESET}"
    hr
    echo -e "   ${C_CYAN}1${C_RESET}  install / converge        (deps · chrome · build · systemd · tls)"
    echo -e "   ${C_CYAN}2${C_RESET}  add / replace tls domain"
    echo -e "   ${C_CYAN}3${C_RESET}  edit configuration        (rate limit · cache · token · db)"
    echo -e "   ${C_CYAN}4${C_RESET}  service status + health"
    echo -e "   ${C_CYAN}5${C_RESET}  update to latest source"
    echo -e "   ${C_CYAN}6${C_RESET}  uninstall"
    hr
    echo -e "   ${C_CYAN}7${C_RESET}  test /health + /config"
    echo -e "   ${C_CYAN}8${C_RESET}  test /v1/ip lookup        (clean json, pretty)"
    echo -e "   ${C_CYAN}9${C_RESET}  test /v1/solver           (earn cf_clearance)"
    echo -e "   ${C_CYAN}0${C_RESET}  self-test all endpoints   (incl. public url)"
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
      7) test_health ;;
      8) test_ip ;;
      9) test_solver ;;
      0) test_all ;;
      q|Q) echo -e "  ${C_GREY}bye 👋${C_RESET}"; exit 0 ;;
      *) warn "pick 0-9 or q" ;;
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
  test-ip)       shift; test_ip ;;
  test-solver)   shift; test_solver ;;
  test-all)      shift; test_all ;;
  menu|"")       action_menu "$@" ;;
  *)             action_menu ;;
esac
