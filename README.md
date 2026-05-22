# nightdrive

Autonomous synthwave music generation & YouTube publishing pipeline.
Cron tick → original song → cover art → animated video → uploaded to YouTube, no human touch.

**Status:** scaffold. See `HANDOFF.md` for the full vision, architecture, and build order.

## Quickstart (when there's actually code)

```bash
# 1. Setup
cp .env.example .env                          # fill in YouTube OAuth + Tailscale endpoints
cp config/nightdrive.toml.example config/nightdrive.toml
mkdir -p /var/lib/nightdrive

# 2. Build
cargo build --release --workspace

# 3. Initialize DB
./target/release/nightdrive-cli db migrate

# 4. Test one-shot
./target/release/nightdrive-orchestrator run-batch --count 1 --dry-run

# 5. Install systemd timer
sudo cp scripts/nightdrive-nightly.{service,timer} /etc/systemd/system/
sudo systemctl enable --now nightdrive-nightly.timer

# 6. Start the livestream supervisor
sudo cp scripts/nightdrive-livestream.service /etc/systemd/system/
sudo systemctl enable --now nightdrive-livestream
```

## Inspecting what it did

```bash
journalctl -u nightdrive-nightly.service -f          # last run logs
./target/release/nightdrive-cli tracks list           # generated tracks
./target/release/nightdrive-cli uploads list          # YouTube uploads
./target/release/nightdrive-cli stream status         # livestream health
```

## Resuming work

1. Read `HANDOFF.md` end to end.
2. Pick the next crate from §9 (Bootstrap order).
3. Each `src/lib.rs` has a `// TODO(nightdrive):` marker — start there.
4. Run `cargo check --workspace` to confirm baseline before touching anything.

## Layout

See `HANDOFF.md` §4.

---

---

---

---

---

---

---

---

---

---

---

## Support This Project

If you find this project useful, consider buying me a coffee! Your support helps me keep building and sharing open-source tools.

[![Donate via PayPal](https://img.shields.io/badge/Donate-PayPal-blue.svg?logo=paypal)](https://www.paypal.me/baal_hosting)

**PayPal:** [baal_hosting@live.com](https://paypal.me/baal_hosting)

Every donation, no matter how small, is greatly appreciated and motivates continued development. Thank you!
