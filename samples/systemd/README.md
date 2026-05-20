# systemd samples

Drop-in unit files for deploying OuroborosFS as a long-running service.

- `ouroboros-node@.service` — one ring node per `%i` instance
  (e.g. `ouroboros-node@7000.service`, `ouroboros-node@7001.service`).
- `ouroboros-gateway.service` — the HTTP + TCP-proxy entry point.
- `ouroboros.env` — shared env file (auth token, RUST_LOG).

## Quick start

```bash
sudo cp samples/systemd/ouroboros-node@.service \
        samples/systemd/ouroboros-gateway.service \
        /etc/systemd/system/
sudo cp samples/systemd/ouroboros.env /etc/default/ouroboros
sudo chmod 600 /etc/default/ouroboros            # secret inside

# Generate a real auth token and edit /etc/default/ouroboros to match.
openssl rand -hex 32

sudo useradd --system --no-create-home --shell /usr/sbin/nologin ouroboros
sudo install -d -o ouroboros -g ouroboros /var/lib/ouroboros

sudo install -o root -g root -m 0755 target/release/ouroboros_fs /usr/local/bin/

sudo systemctl daemon-reload
sudo systemctl enable --now ouroboros-node@7000 \
                            ouroboros-node@7001 \
                            ouroboros-node@7002 \
                            ouroboros-gateway
```

The ring still needs to be wired (`NODE NEXT`) once on first boot — see
the README §3.3 or `docs/operations.md` for the operational walkthrough.
