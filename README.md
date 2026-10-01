<div align="center">

# ⚡ cf-turnstile-solver

**Cloudflare Turnstile + IUAM solver and IP-intelligence API in one zero-dependency Rust binary.**

Raw CDP — no webdriver, no node, no serde, no tokio. One ~800 KB binary plus a Chrome.
Runs happily on a **1 vCPU / 881 MiB** VPS: the browser pool auto-sizes to your RAM.

</div>

```
███████╗ ██████╗ ██╗    ██╗███████╗██████╗ ██████╗
██╔════╝██╔═══██╗██║    ██║██╔════╝██╔══██╗██╔══██╗
███████╗██║   ██║██║ █╗ ██║█████╗  ██████╔╝██████╔╝
╚════██║██║   ██║██║███╗██║██╔══╝  ██╔══██╗██╔══██╗
███████║╚██████╔╝╚███╔███╔╝███████╗██║  ██║██████╔╝
╚══════╝ ╚═════╝  ╚══╝╚══╝ ╚══════╝╚═╝  ╚═╝╚═════╝
```

## Install — one command

```bash
curl -fsSL https://raw.githubusercontent.com/maxieyy/cf-turnstile-solver/main/install.sh | bash
```

The installer is an interactive menu: it installs system deps + Rust + Chrome,
asks whether to persist to **sqlite**, asks for your **TLS domain** (or skips),
builds the binary, creates a **systemd** service, and puts **Caddy** in front
for automatic HTTPS. The API itself always binds `127.0.0.1` — only Caddy
faces the internet.

**It is fully idempotent** — re-run it any time (same command); it detects the
existing install, pulls the latest source, rebuilds if needed, and converges
to a good state without touching your config or database. Caddy falls back to
an official static binary when the distro package is unavailable.

Menu includes **live endpoint testers** (pretty-printed): `/health` +
`/config`, `/v1/ip` lookups, `/v1/solver` clearance runs, and a one-shot
self-test of everything including the public URL.

Non-interactive:

```bash
curl -fsSL https://raw.githubusercontent.com/maxieyy/cf-turnstile-solver/main/install.sh \
  | bash -s -- install --domain api.example.com --db yes --token s3cret
```

CLI subcommands (all idempotent):

```bash
install | tls | config | status | update | uninstall | test-ip | test-solver | test-all | menu
```

Examples:

```bash
# pull latest + rebuild + restart (the safe update)
curl -fsSL .../install.sh | bash -s -- update

# quick ip lookup from the terminal, pretty-printed
curl -fsSL .../install.sh | bash -s -- test-ip
```

Or run the installer's menu later:

```bash
bash /opt/solver/src/install.sh
```

## Endpoints

| Method | Path | Body | Description |
|--------|------|------|-------------|
| POST | `/v1/solver` | `{"url":"https://site.com"}` | Earn a fresh `cf_clearance` (+ cookies, UA, egress IP) |
| POST / GET | `/v1/ip` | `{"ip":"1.2.3.4"}` or `?ip=1.2.3.4` | IP intelligence, clean JSON (cached, see below) |
| POST | `/turnstile` | `{"url":"…","sitekey":"0x…"}` | Legacy: Turnstile token |
| POST | `/iuam` | `{"url":"https://site.com"}` | Legacy: IUAM clearance |
| POST | `/cloudflare` | `{"mode":"…", …}` | Legacy: mode in body |
| GET | `/health` | — | Pool capacity + cache/db diagnostics |
| GET | `/config` | — | Effective configuration |

Auth (optional): set `"api_token"` in `/etc/solver/config.json`, then send
`Authorization: Bearer <token>`. Rate limiting (token bucket) is per token or
per client IP and is configured in the same file.

### POST /v1/solver

```bash
curl -X POST https://solver.maxwell.deals/v1/solver \
  -H "content-type: application/json" \
  -d '{"url":"https://nowsecure.nl"}'
```

```json
{
  "headers": {
    "Cookie": "cf_clearance=155jEz2BCC8oFRCOu0x8...",
    "User-Agent": "Mozilla/5.0 (Linux; Android 16; K) AppleWebKit/537.36 ..."
  },
  "ip": "45.152.31.27",
  "elapsed": "2.87s",
  "status": "completed"
}
```

Replay the returned `Cookie` + `User-Agent` with a matching client
(e.g. `curl_cffi` impersonation) — the clearance is tied to IP + TLS
fingerprint. Proxies are supported per request: `"proxy":"http://user:pass@host:port"`.

### POST /v1/ip

```bash
curl -X POST https://solver.maxwell.deals/v1/ip \
  -H "content-type: application/json" \
  -d '{"ip":"197.157.165.49"}'
```

```json
{
  "ip": "197.157.165.49",
  "decimal": 3315442993,
  "hostname": "49-165-157-197.r.airtel.co.rw",
  "asn": 327707,
  "isp": "Airtel Rwanda Ltd",
  "services": [],
  "country": "Rwanda",
  "region": "Ville de Kigali",
  "city": "Kigali",
  "latitude": -1.9501,
  "longitude": 30.0588,
  "cached": false,
  "source": "miss"
}
```

Data source: whatismyipaddress.com — the solver passes the Cloudflare check in
a real headless Chrome, then reads the details straight out of the live DOM in
the same cleared browser context (nothing is hardcoded; every lookup earns its
own clearance).

## Caching — invisible by design

`/v1/ip` is served through a two-tier cache that users never notice: there is
no cache endpoint and no cache field to think about — repeat questions get
instant answers.

- **L1** in-process TTL map (default 15 min) — repeat lookups answer in
  milliseconds.
- **L2** sqlite (optional, chosen at install time) — survives restarts; a
  cold process answers from disk instead of re-solving.
- **Stale-while-revalidate** — an expired entry is still served instantly while
  a background thread refreshes it for the *next* request. Users never wait on
  a refresh.
- **Singleflight / stampede protection** — 1000 concurrent requests for the
  same IP produce exactly **one** upstream fetch; the rest share the result.
- **Stale-if-error** — if upstream breaks, slightly-old answers are served
  rather than errors.
- **Invalidation** — automatic: TTL expiry, lazy purge, janitor sweep every
  10 min, LRU-style eviction at `max_entries`.

`source` in the response (`miss | memory_hit | db_hit | coalesced |
stale_revalidated`) is informational, for operators. `/health` exposes the full
hit/miss/revalidation counters.

## Configuration

`/etc/solver/config.json` (created by the installer):

```json
{
  "host": "127.0.0.1",
  "port": 8907,
  "api_token": null,
  "db_path": "/var/lib/solver/solver.db",
  "rate_limit": { "enabled": true, "capacity": 10, "refill_per_sec": 0.5 },
  "cache": { "enabled": true, "ttl_secs": 900, "max_entries": 2048, "stale_grace_secs": 3600 },
  "proxies": []
}
```

`browsers` and `tabs` are **omitted on purpose**: by default the binary
auto-sizes to the machine (1 browser + 4 tabs on 881 MiB, more on bigger boxes).
Set them explicitly if you want to override. Every field can also be set via
env: `HOST`, `PORT`, `BROWSERS`, `TABS`, `API_TOKEN`, `PROXIES` (comma
separated), `DB_PATH`, `CACHE_TTL_SECS`, `RATE_LIMIT_CAPACITY`,
`RATE_LIMIT_REFILL_PER_SEC`, `CONFIG` (config file path).

For low-RAM boxes the installer offers to create a 2G swapfile for the
(one-time) Rust build.

## Building from source

```bash
git clone https://github.com/maxieyy/cf-turnstile-solver
cd cf-turnstile-solver/SOLVER
cargo build --release --features db   # db = sqlite log + persistent cache
CHROME_BIN=/usr/bin/google-chrome ./target/release/solver
```

> On Windows, add `[target.x86_64-pc-windows-msvc]\nlinker = "rust-lld"` to
> `.cargo/config.toml` if your PATH has a conflicting `link.exe` (Git's
> coreutils), and install the VS C++ build tools or use the GNU toolchain.

Without `--features db` you get the same API with no sqlite dependency and a
smaller binary. Docker: `docker build --build-arg FEATURES=db -t solver .` (see
`Dockerfile`).

## How it works

Two Chrome processes stay warm (count auto-sized). Each solve opens a fresh
isolated browser context — own cookie jar, no cross-solve leakage. The solver
loads a stub page that renders the real Turnstile widget and clicks through it,
or loads the real target page for IUAM, and reads `cf_clearance` from the
cookie jar. `/v1/ip` then keeps that same cleared context alive just long
enough to read the parsed details from the live DOM. Contexts are destroyed
afterwards.

## License

MIT © [maxieyy](https://github.com/maxieyy). See [LICENSE](./LICENSE).
