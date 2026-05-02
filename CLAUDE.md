# Project notes for Claude

## Project layout

```
src/main.rs               firmware entrypoint, Wi-Fi + HTTPS + OTA loop
src/ota.rs                OTA polling, manifest fetch, blob streaming
src/sig.rs                cosign Sigstore Bundle verification
src/trust.rs              NVS-loaded trust config (identities + Sigstore CAs)
trust/                    Sigstore Fulcio root + intermediate certs (provisioned)
tools/publisher/          host-side tool that pushes signed OCI artifacts
tools/provision/          host-side tool that builds + flashes NVS partition
.github/workflows/        ci.yml (PRs) + publish.yml (push to main)
Cargo.toml                firmware deps
partitions.csv            OTA-capable partition table (1.94 MB app slots)
sdkconfig.defaults.in     ESP-IDF kconfig (Makefile substitutes paths)
Makefile                  build / flash / monitor / provision / publish entrypoints
provisioning.toml.example template for per-device NVS values
ota.md                    full OTA system documentation
provisioning-plan.md      provisioning design + future Secure Boot v2 work
eink-plan.md              planned e-ink display work
logs-plan.md              planned GCP Cloud Logging integration
notes.txt                 internal design notes + setup gotchas (Python 3.12
                          shim, partition-table CMake quirk, etc.)
```

## Conventions established in this repo

- **Plans live in the repo** as `*-plan.md` (per-feature) and `ota.md`
  (descriptive doc once a system is operational). Not in Claude memory.
- **Cargo.lock IS tracked** for all three crates here — they're all
  binaries.
- **Don't use "and" in Make target names** — chain existing targets
  instead (`make publish monitor`, not `make publish-and-monitor`).
- **Env-setup scripts** (e.g. espup's `export-esp.sh`) live in the
  project dir, not `~`.
- **GHCR auth** uses classic PATs with `write:packages` (fine-grained
  PATs don't expose a Packages permission for user-owned packages).
