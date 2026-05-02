# Investigate `no_std` + `esp-hal`

## Short answer

Probably yes, but **not by incrementally removing a `std` feature flag from the current app**.

This firmware is built around [`esp-idf-svc`](https://docs.rs/esp-idf-svc/latest/esp_idf_svc/) with the `std` feature enabled in [`Cargo.toml`](Cargo.toml), and it also enables `build-std = ["std", "panic_abort"]` in [`.cargo/config.toml`](.cargo/config.toml). The current code uses:

- `std::thread` / `std::time::Duration` in [`src/main.rs`](src/main.rs)
- `esp_idf_svc` Wi-Fi + HTTP client wrappers in [`src/main.rs`](src/main.rs)
- `esp_idf_svc` HTTP, NVS, and OTA services in [`src/ota.rs`](src/ota.rs)
- heap-backed `String` / `Vec`-heavy parsing and verification in [`src/ota.rs`](src/ota.rs) and [`src/sig.rs`](src/sig.rs)

So a move to `no_std` would be a **port to a different platform stack**, not a small cleanup.

## What exists upstream

- [`esp-hal`](https://github.com/esp-rs/esp-hal/blob/main/README.md) explicitly describes itself as a bare-metal **`no_std`** HAL.
- [`esp-wifi`](https://crates.io/crates/esp-wifi) is also `no_std` and provides Wi-Fi support on top of the `esp-hal` ecosystem.
- [`esp-bootloader-esp-idf`](https://github.com/esp-rs/esp-hal/blob/main/esp-bootloader-esp-idf/src/ota.rs) provides low-level OTA partition/state handling that can work in a `no_std` system using the ESP-IDF bootloader layout.
- [`esp-storage`](https://crates.io/crates/esp-storage) exists for raw flash access in `no_std`.

That means the basic ingredients exist for a `no_std` rewrite.

## What would have to change in this repo

The current app depends on several ESP-IDF services that would need replacements:

| Current repo usage | Current implementation | `no_std` replacement direction |
| --- | --- | --- |
| Wi-Fi connect | `esp-idf-svc::wifi` | `esp-hal` + `esp-wifi` / `esp-radio` |
| HTTPS GETs | `esp-idf-svc::http::client::EspHttpConnection` + IDF TLS bundle attach | `embassy-net`/`smoltcp` + an embedded HTTP client + a separate TLS stack |
| OTA write + rollback state | `esp-idf-svc::ota::EspOta` | `esp-bootloader-esp-idf` + direct flash/partition handling |
| NVS config/digest storage | `esp-idf-svc::nvs::EspNvs` | raw flash / custom storage layer / `esp-storage`-based design |
| Background work model | `std::thread` | Embassy tasks or another executor/scheduler |

## Biggest risk / gap

The hardest part is not GPIO or timing; it is the current **networking + TLS + OTA** stack.

Today the firmware gets a lot “for free” from ESP-IDF:

- synchronous Wi-Fi setup
- synchronous HTTPS client
- certificate bundle attach (`esp_crt_bundle_attach`)
- NVS wrappers
- OTA helpers

The `esp-hal` ecosystem has the pieces for `no_std`, but the repo would need a different application architecture:

- probably async (`embassy`) instead of blocking threads
- a new HTTP client path
- a chosen TLS story
- new persistent-state plumbing for OTA bookkeeping

As one example of the kind of stack people are assembling in `no_std`, [`MindsHub/radioala-esp`](https://github.com/MindsHub/radioala-esp/blob/main/Cargo.toml) combines `esp-hal`, `embassy-net`, `smoltcp`, and `reqwless`; notably, its `embedded-tls`/mbedtls path is still commented out in `Cargo.toml`, which is a good signal that TLS integration is where things get real.

## Recommendation

I would **not** try to convert this codebase in-place.

Instead:

1. Keep the current `esp-idf-svc` app as the working baseline.
2. Build a tiny proof-of-concept `no_std` binary separately that only:
   - boots on this ESP32 board,
   - joins Wi-Fi,
   - does one HTTP or HTTPS request,
   - writes/reads one small piece of persistent state,
   - proves OTA slot writing is viable.
3. Compare binary size, RAM use, and implementation complexity before deciding whether the full port is worth it.

## Bottom line

`no_std` + `esp-hal` looks **possible in principle**, and it may reduce image size / free resources, but for this repository it is a **rewrite-level migration** because the current firmware is tightly coupled to `std` and ESP-IDF service wrappers.
