# Handoff — deploy the arm64 build to `ivarvps` (script brief)

Written for the agent that writes the deploy script. Read this before touching
the host; the ordering below is load-bearing, not stylistic.

## 0. Where things stand

| | |
|---|---|
| Repo HEAD | `986a7d590 fix(web): fold the context meter in one tested place` |
| `dist/bin/aarch64-unknown-linux-gnu/zeroclaw` | **stale** — `sha256 3b6cac41…`, built 10-26 22:54, i.e. **before** the anchored in-turn guard (`0762173f3`) and the web meter fold (`986a7d590`) |
| Host | `ivarvps`, aarch64, user `ubuntu` |
| Installed binary | `~/.cargo/bin/zeroclaw` — same stale `3b6cac41…` |
| Service | systemd **user** unit `zeroclaw.service`, `ExecStart=%h/.cargo/bin/zeroclaw daemon`, `Restart=always`, drop-ins in `~/.config/systemd/user/zeroclaw.service.d/` |
| Config | `~/.zeroclaw/config.toml`; `[gateway] host=0.0.0.0 port=42617` |
| Dashboard | served from the **embedded** copy (`--features embedded-web`); on-disk `web/dist` is never served under that feature |

**Therefore step 1 is a rebuild, not a copy.** The existing `dist/bin` artifact
must be replaced by one built from `986a7d590` (or later) before anything is
deployed.

## 1. Build (on the dev host, not on `ivarvps`)

```bash
bash scripts/dev/build-cli-local.sh          # aarch64-unknown-linux-gnu, features embedded-web,plugins-wasm-cranelift
```

Constraints:

- Never hand-roll `docker run … cargo build`: default features do **not** embed
  `web/dist`, and the resulting binary serves a stale on-disk dashboard.
- The script rebuilds `web/dist`, asserts the current `assets/index-*.js`
  fingerprint is inside the ELF, collects into `dist/bin/<triple>/zeroclaw`, then
  reclaims `target/`. Its reclaim now removes root-owned `target/` from inside a
  container; if it still reports residue, do **not** `docker system prune -a`.
- Record three values from the build output: the new `sha256`, the embedded
  fingerprint (`assets/index-<hash>.js`), and the binary size.

## 2. Stale frontend caches — purge **before** the restart

Two independent classes. Both must be handled; the script must not conflate them.

### 2a. Server-side dashboard trees (files)

`resolve_web_dist_dir` auto-detects, in this order, and the first hit wins:

1. `web/dist` relative to the **daemon's working directory** (unit has no
   `WorkingDirectory`, so systemd uses `%h` → `/home/ubuntu/web/dist`)
2. `<dir of the running executable>/web/dist` → `~/.cargo/bin/web/dist`
3. `/zeroclaw-data/web/dist`
4. `/usr/share/zeroclawlabs/web/dist`
5. `~/.local/share/zeroclaw/web/dist`

On this host step 5 exists and carries the **Aug 2** dashboard
(`assets/index-D7KfOnNy.js`, no `ring_detail_trim_budget` key). A binary built
with `embedded-web` outranks all of them at request time, so this tree is not
what the browser got — but it is exactly the trap that bites the next
non-embedded build or installer, and it is what makes "which dashboard is live?"
ambiguous during a debug session. The deploy script should remove every
auto-detect hit, not only the one that resolves today:

```bash
for d in "$PWD/web/dist" "$HOME/web/dist" "$(dirname "$(command -v zeroclaw)")/web/dist" \
         /zeroclaw-data/web/dist /usr/share/zeroclawlabs/web/dist \
         "$HOME/.local/share/zeroclaw/web/dist" "$HOME/.zeroclaw/src/web/dist"; do
  [ -e "$d" ] && sudo rm -rf -- "$d"     # sudo only where the path is not owned by ubuntu
done
```

Do **not** delete `dist/bin/**` on the dev host, and do not delete
`~/.zeroclaw/src` wholesale — only its `web/dist`.

### 2b. Browser page caches (clients)

The script cannot clear a browser, but it must stop the deploy from looking
successful while a stale tab is still on screen. Facts:

- `index.html` is served `Cache-Control: no-cache`; hashed assets are
  `public, max-age=31536000, immutable`. A new build therefore gets a **new**
  asset name and a reload picks it up — but a tab that was open across the swap
  keeps the **old** JS in memory (the old hash name is still a valid cache hit).
- The dashboard registers **no service worker** (verified: the served
  `index.html` contains no `sw.js`/`serviceWorker` reference). Do not add
  `navigator.serviceWorker` cache logic to the deploy; just reload.
- Symptoms of a stale tab, which the script should treat as a *client* problem
  and say so: the ring panel reads **"trim trigger"** (old wording) instead of
  **"trim budget"**, and the ring keeps a pre-trim number.

Post-deploy instruction the script must print verbatim:

```
Hard-reload the dashboard (Ctrl+Shift+R). If it still shows "trim trigger",
clear Site Data for this origin (DevTools -> Application -> Storage -> Clear
site data) and reload; a tab opened before this deploy keeps the old bundle.
```

## 3. Deploy sequence (order matters)

1. **Stop the service first.** `Restart=always` will otherwise resurrect the
   process from `~/.cargo/bin/zeroclaw` while the file is being replaced.
   ```bash
   systemctl --user stop zeroclaw
   ```
   Non-interactive `ssh` has no session bus: export
   `XDG_RUNTIME_DIR=/run/user/$(id -u)` (and `DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus`)
   before any `systemctl --user`, or run the whole script inside
   `systemd-run --user` / a login shell.
2. Copy to a staging path, verify, then install:
   ```bash
   scp -p dist/bin/aarch64-unknown-linux-gnu/zeroclaw ivarvps:~/Download/zeroclaw
   ssh ivarvps 'sha256sum ~/Download/zeroclaw'           # must equal the build's sha
   ssh ivarvps 'install -m 0755 ~/Download/zeroclaw ~/.cargo/bin/zeroclaw'
   ```
   Verify the installed copy, not the staging copy:
   `sha256sum ~/.cargo/bin/zeroclaw`.
3. Purge the dashboard trees from §2a (now, while stopped, so nothing can be
   read mid-delete).
4. Start and wait for readiness:
   ```bash
   systemctl --user start zeroclaw
   for i in $(seq 1 30); do curl -sf -o /dev/null http://127.0.0.1:42617/ && break; sleep 1; done
   ```
5. Verify (§4). On any mismatch: stop, restore the previous binary, start, and
   report — do not leave the host in a half-verified state.

## 4. Verification — fail loudly, all five checks

```bash
# a) the running process is the artifact you installed
readlink -f /proc/$(pgrep -o -f 'zeroclaw daemon')/exe   # -> /home/ubuntu/.cargo/bin/zeroclaw
sha256sum /proc/$(pgrep -o -f 'zeroclaw daemon')/exe      # -> the build's sha

# b) the served document points at the new bundle
curl -s http://127.0.0.1:42617/ | grep -oE '/_app/assets/index-[A-Za-z0-9_-]+\.js'
#    -> must be the fingerprint printed by build-cli-local.sh

# c) the new wording is live (the old bundle has none of it)
curl -s http://127.0.0.1:42617/_app/assets/index-<hash>.js | grep -c 'ring_detail_trim_budget'   # -> >= 1

# d) the dashboard log line says embedded, not filesystem
journalctl --user -u zeroclaw -n 40 --no-pager | grep -i 'Web dashboard'
#    -> "serving embedded assets"

# e) a turn-start trim re-prices the meter (requires a real turn; only do it
#    if the user is driving the session) — see docs/handoff-2026-10-27-in-turn-guard.md
```

The binary answers `~/Download/zeroclaw --version` → `zeroclaw 1.2.5` on this
release; the version alone does not identify a build, the sha256 does.

## 5. Do not

- Do not copy a binary whose sha256 differs from the one the build printed.
- Do not restart before the dashboard trees are purged.
- Do not `docker system prune -a`, and do not delete `~/.zeroclaw/{agents,data,state}`
  — history, sessions and credentials live there.
- Do not "fix" a stale-looking dashboard by editing `~/.local/share/zeroclaw/web/dist`:
  under `embedded-web` it is dead weight, and re-creating it re-arms the trap.
- Do not treat a client-side stale tab as a deploy failure: checks (a)–(d) prove
  the server, and §2b tells the user what to clear.
