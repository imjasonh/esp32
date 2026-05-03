# E-ink display — plan

The Inland ESP-WROOM-32 dev board doesn't have a software-controllable LED
(see [`setup.md`](setup.md)), so the visible-output story for this
project is an e-ink display. This file captures the hardware choice,
the Rust graphics stack we'll use, and a shortlist of projects to
build on top.

## Hardware

- **Display**: Inland 2.13" e-ink (Micro Center SKU 632694).
  - 250×122 mono, SPI, ~Waveshare 2.13" rebrand
  - 8-pin male header: `VCC`, `GND`, `DIN`, `CLK`, `CS`, `DC`, `RST`, `BUSY`
  - Pre-soldered headers — no soldering required
- **Refresh characteristics** (SSD1680 driver, typical for this panel):
  - Full refresh ~2s, with the black/white flash that clears ghosting
  - Partial refresh ~300ms, no flash, but ghosting accumulates
  - Run a full every 5–10 partials
  - No grayscale, no animation. "Update once, look at it for a while."
- **Supporting hardware**: half-size solderless breadboard, Dupont jumper
  assortment (F-F at minimum). Both boards are 3V3, no level shifter needed.

Wiring sketch (pins on the ESP32 are conventional for SPI2/HSPI, adjust
as needed once the panel arrives):

```
e-ink   ESP32
-----   -----
VCC  -> 3V3
GND  -> GND
DIN  -> GPIO23 (MOSI)
CLK  -> GPIO18 (SCK)
CS   -> GPIO5
DC   -> GPIO17
RST  -> GPIO16
BUSY -> GPIO4
```

## Rust graphics stack

All sit on top of the existing `esp-idf-svc` setup — `Cargo.toml`
already pins the core set:

| Crate | Pinned | Notes |
|-------|--------|-------|
| `embedded-graphics` | 0.8.2 | Primitives + `DrawTarget` trait. Stable since 2023. |
| `epd-waveshare` | 0.6.0 | SSD1680 driver. **Disable default features** — defaults pull `linux-embedded-hal`, which won't cross-compile to `xtensa-esp32-espidf`. Re-enable `graphics` + `epd2in13_v3` (the V3/V4 init path; module is shared with V2 internally). |
| `embedded-hal` | 1.0 | `epd-waveshare` 0.6 hard-requires the 1.0 trait set. `esp-idf-hal` exposes both 0.2 and 1.0 simultaneously, so this just dictates which traits we import. |
| `embedded-hal-bus` | 0.2 | `ExclusiveDevice` wraps an SPI bus + CS pin into the `SpiDevice` trait that `Epd2in13::new` consumes. |
| `u8g2-fonts` | 0.7.2 | Larger / nicer fonts than e-g's built-ins. We pre-select Small/Medium/Large from this set in [`src/display.rs`](../src/display.rs) so only the referenced font tables get linked. |

Not yet pulled in but easy to add when the need arises:

- **`embedded-text`** (text wrapping / bounded multiline)
- **`embedded-layout`** (stack + alignment helpers)
- **`tinybmp`** (1-bit BMPs from `include_bytes!`)
- **`embedded-iconoir`** (Iconoir glyph set)

### Why not `ssd1680` direct?

There's a third-party `ssd1680` crate that talks to the same controller
without the Waveshare-specific layer, but as of 0.2.0 it explicitly
doesn't support partial refresh — and partial refresh is the whole
reason this panel is interesting on a battery budget. Stay on
`epd-waveshare`.

### Heavier option: Slint

[Slint](https://slint.dev/) has an MCU backend with explicit e-ink
support. Write `.slint` markup, get live layout preview. Workable on
the ESP32-WROOM-32 but noticeable flash/RAM footprint. Reach for it
only if iterating on layout becomes a bottleneck with the
embedded-graphics stack.

## Abstraction: `Panel` + `Scene`

Implemented in [`src/display.rs`](../src/display.rs). Two types and a
small builder API; the rest is policy.

```rust
let mut panel = display::Panel::stub();          // (or ::new(...) once wired)
panel.present(
    &display::Scene::new()
        .text("esp32-blinky", Point::new(2, 0), Font::Medium)
        .line(Point::new(0, 18), Point::new(249, 18))
        .text(format!("ip: {}", ip), Point::new(2, 24), Font::Small),
)?;
```

What the abstraction handles for you:

- **Content diff.** `Scene` hashes its element list. Re-presenting an
  identical scene is a no-op — no SPI traffic, no ghosting nudge.
- **Refresh policy.** First present after boot → full refresh.
  Every 8th partial → forced full to clear ghosting. `Scene::force_full()`
  → caller-requested full. Otherwise → partial refresh (~300ms).
- **Framebuffer ownership.** `Panel` owns the `Display2in13` buffer
  for the lifetime of the program; `present` clears + redraws into it
  on every change. The same buffer is what gets shipped to the EPD
  (via `update_and_display_frame` for full, `update_partial_frame` for
  partial), so no extra copy.
- **Stub mode.** Until the hardware lands, `Panel::stub()` runs the
  whole renderer end-to-end (so layout iteration works today and isn't
  thrown away) and just skips the EPD push. It logs `on_pixels` per
  frame as a sanity check that the renderer actually drew something.

What you write yourself, scene by scene: the `(x, y)` of each
element, the `Font` size, and an explicit `force_full()` if you want
to fight ghosting on a particular frame. No layout engine yet — pull
in `embedded-layout` if positioning gets tedious.

### Critical `epd-waveshare` 0.6 gotcha to remember when wiring hardware

Partial refresh keeps a "compare buffer" inside the SSD1680. After
**every** full refresh, we have to call
`epd.set_partial_base_buffer(spi, delay, framebuffer)` to reseed it,
or the next partial draw will display garbage. The hardware-mode
`present` will need to handle this in the `force_full → partial`
transition. Easy to miss; flag for the hardware-bring-up commit.

## What's done vs. pending hardware

Done in this branch (no panel needed):

- [x] Pinned crate versions in `Cargo.toml`
- [x] `Scene` builder API + `Panel::stub()`
- [x] `embedded-graphics` + `u8g2-fonts` renderer wired end-to-end
      against the real `Display2in13` framebuffer
- [x] Refresh-policy state machine (diff hash + partial/full counter)
- [x] `display::boot_info_scene()` demo, called from `main.rs` after
      Wi-Fi connects

Pending until the panel is in hand:

- [ ] `Panel::new(spi, dc, rst, busy, delay)` constructor — wraps SPI
      bus + CS via `embedded_hal_bus::spi::ExclusiveDevice`, builds
      `Epd2in13::new(...)`, switches to landscape rotation. Replaces
      `Backend::Stub` with `Backend::Hardware { epd, spi, delay }`.
- [ ] Hardware `present` path: `set_refresh(Full|Quick)` →
      `update_and_display_frame` or `update_partial_frame` →
      `set_partial_base_buffer` after every full.
- [ ] Verify the GPIO assignment in this doc against an actual ESP32
      pinout (tentative: SCK 18 / MOSI 23 / CS 5 / DC 17 / RST 16 /
      BUSY 4).

### Desktop simulator (bigger follow-up)

`embedded-graphics-simulator` (SDL-based) is the natural way to
preview scenes without flashing, but it doesn't cross-compile to
`xtensa-esp32-espidf`. The clean path is a workspace split:

- Move `Scene` + `render_scene` + `Font` enum into a `eink-ui/`
  library crate.
- Firmware (`src/display.rs`) and a new `tools/eink-sim/` host crate
  both depend on it; the host crate imports
  `embedded-graphics-simulator` and renders scenes into an SDL window
  (or PNG, for CI screenshots).

Worth doing once we're iterating on more than one screen layout. Not
worth doing today.

## Project shortlist

Ordered by cool : effort ratio.

1. **Wi-Fi info screen** — weather, next calendar event, GitHub PR count,
   refreshed every 5–10 minutes. **Best first project**: smallest
   end-to-end exercise of Wi-Fi + HTTP + JSON + e-ink rendering. Once it
   works, the rest are variations on the same skeleton.
2. **Build status badge** — last commit's CI state for one or more repos.
   Sits on the desk, nags when CI breaks.
3. **Pomodoro timer** — two physical buttons, big "WORK 17:32" / "BREAK
   04:00" text. E-ink wins because no backlight to distract.
4. **Now-playing card** — Spotify Web API. Refresh slowness fits since
   tracks last minutes.
5. **Door / desk status sign** — "available / heads-down / on a call",
   toggled from phone via tiny HTTP server on the ESP32. Battery + e-ink
   = days of runtime.
6. **Home Assistant / MQTT subscriber** — display whatever the home
   automation system publishes.
7. **Conference badge / desk name tag** — name + handle + QR code,
   updateable over Wi-Fi.
8. **Status dashboard for a personal service** — uptime, last-deploy
   time, error count from one of Jason's own services.

## Crate stack already in place

`Cargo.toml` has `esp-idf-svc` 0.52 with `binstart` + `native` (covers
Wi-Fi, HTTP client, NTP, NVS) plus the e-ink stack pinned in the table
above. The SPI driver comes from `esp_idf_svc::hal::spi` (re-exported
from `esp-idf-hal`); we don't depend on `esp-idf-hal` directly.
