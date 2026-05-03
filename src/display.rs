//! E-ink display abstraction (Inland 2.13" / Waveshare V3, SSD1680).
//!
//! Two layers, deliberately small:
//!
//! - [`Scene`] — declarative description of what should be on screen.
//!   Build with the chained-builder API (`Scene::new().text(...).line(...)`),
//!   pass to `Panel::present`. The scene owns its strings; rendering
//!   reads the scene at present-time, no intermediate drawing context.
//! - [`Panel`] — owns the framebuffer (and, once hardware is wired, the
//!   SPI device + EPD driver). `present` diffs the new scene against
//!   the last-presented one (cheap content hash) and decides full-vs-
//!   partial refresh based on a counter. Identical scenes are no-ops.
//!
//! Refresh policy mirrors what e-ink panels demand:
//! - First present after boot → full refresh (clears any prior content)
//! - Every [`FULL_REFRESH_EVERY`] partials → forced full to clear ghosting
//! - `Scene::force_full()` → caller-requested full
//! - Otherwise → partial refresh (~300ms, accumulates ghosting)
//!
//! ## Hardware status
//!
//! The panel isn't wired up yet (May 2026). [`Panel::stub`] is the only
//! constructor; it allocates the same framebuffer the hardware path
//! will use and runs the renderer end-to-end, so layout work today is
//! not throwaway. When the panel arrives we add `Panel::new(spi, dc,
//! rst, busy, delay)` and a `Backend::Hardware` variant whose
//! `present_to_panel` calls `Epd2in13::update_and_display_frame` /
//! `update_partial_frame`. The public API doesn't change.
//!
//! ## Desktop simulator (follow-up)
//!
//! `embedded-graphics-simulator` is SDL-based and won't cross-compile
//! to `xtensa-esp32-espidf`. To get a desktop preview without flashing,
//! the path is to extract the scene/renderer code into a small library
//! crate and add a `tools/eink-sim/` host binary that depends on it.
//! Not done in this pass; see `docs/eink-plan.md`.

use anyhow::{anyhow, Result};
use embedded_graphics::{
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
};
use epd_waveshare::epd2in13_v3::Display2in13;
// `Display` is the trait that provides `set_rotation` + `buffer` on
// the framebuffer struct; `DisplayRotation` is the enum it consumes.
use epd_waveshare::graphics::{Display, DisplayRotation};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use u8g2_fonts::{fonts, types::*, FontRenderer};

/// Panel pixel dimensions in landscape orientation.
pub const WIDTH: u32 = 250;
pub const HEIGHT: u32 = 122;

/// After this many partial refreshes, force a full refresh to clear
/// accumulated ghosting. SSD1680 datasheet recommends "every 5–10";
/// 8 keeps clock-style updates feeling responsive without obvious decay.
const FULL_REFRESH_EVERY: u32 = 8;

// ---------------------------------------------------------------------------
// Scene
// ---------------------------------------------------------------------------

/// Declarative description of one frame.
///
/// Build with the chained-builder API. `present` consumes a `&Scene`,
/// so the same scene can be rebuilt and re-presented cheaply (the
/// content-hash check skips the actual draw if nothing changed).
#[derive(Debug, Default)]
pub struct Scene {
    elements: Vec<Element>,
    force_full: bool,
}

#[derive(Debug)]
enum Element {
    Text {
        text: String,
        position: Point,
        font: Font,
    },
    Rect {
        rect: Rectangle,
        fill: bool,
    },
    Line {
        start: Point,
        end: Point,
    },
}

/// Pre-selected font sizes. Add variants here rather than threading
/// arbitrary u8g2 font types through the public API — keeps the
/// builder ergonomic and the binary smaller (only referenced font
/// tables get linked in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Font {
    /// helvR08 — small caption / metadata text (~8px cap height).
    Small,
    /// helvR12 — body text (~12px cap height).
    Medium,
    /// helvB18 — headline / numerals (~18px cap height, bold).
    Large,
}

impl Scene {
    pub fn new() -> Self {
        Self::default()
    }

    /// Force a full refresh on the next present, even if the content
    /// is identical to what's currently shown. Use when you suspect
    /// ghosting (e.g., after long idle) or want a guaranteed clean frame.
    pub fn force_full(mut self) -> Self {
        self.force_full = true;
        self
    }

    pub fn text(mut self, text: impl Into<String>, position: Point, font: Font) -> Self {
        self.elements.push(Element::Text {
            text: text.into(),
            position,
            font,
        });
        self
    }

    pub fn rect_outline(mut self, rect: Rectangle) -> Self {
        self.elements.push(Element::Rect { rect, fill: false });
        self
    }

    pub fn rect_filled(mut self, rect: Rectangle) -> Self {
        self.elements.push(Element::Rect { rect, fill: true });
        self
    }

    pub fn line(mut self, start: Point, end: Point) -> Self {
        self.elements.push(Element::Line { start, end });
        self
    }

    /// Stable-within-a-boot content hash. `DefaultHasher`'s seed
    /// randomizes per-process, but we only ever compare against
    /// hashes computed during the same boot, so that's fine.
    fn content_hash(&self) -> u64 {
        let mut h = DefaultHasher::new();
        for el in &self.elements {
            match el {
                Element::Text {
                    text,
                    position,
                    font,
                } => {
                    0u8.hash(&mut h);
                    text.hash(&mut h);
                    position.x.hash(&mut h);
                    position.y.hash(&mut h);
                    font.hash(&mut h);
                }
                Element::Rect { rect, fill } => {
                    1u8.hash(&mut h);
                    rect.top_left.x.hash(&mut h);
                    rect.top_left.y.hash(&mut h);
                    rect.size.width.hash(&mut h);
                    rect.size.height.hash(&mut h);
                    fill.hash(&mut h);
                }
                Element::Line { start, end } => {
                    2u8.hash(&mut h);
                    start.x.hash(&mut h);
                    start.y.hash(&mut h);
                    end.x.hash(&mut h);
                    end.y.hash(&mut h);
                }
            }
        }
        h.finish()
    }
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

fn render_scene<D>(target: &mut D, scene: &Scene) -> Result<()>
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    for el in &scene.elements {
        match el {
            Element::Text {
                text,
                position,
                font,
            } => {
                let renderer = match font {
                    Font::Small => FontRenderer::new::<fonts::u8g2_font_helvR08_tf>(),
                    Font::Medium => FontRenderer::new::<fonts::u8g2_font_helvR12_tf>(),
                    Font::Large => FontRenderer::new::<fonts::u8g2_font_helvB18_tf>(),
                };
                renderer
                    .render(
                        text.as_str(),
                        *position,
                        VerticalPosition::Top,
                        FontColor::Transparent(BinaryColor::On),
                        target,
                    )
                    .map_err(|e| anyhow!("u8g2 render: {:?}", e))?;
            }
            Element::Rect { rect, fill } => {
                let style = if *fill {
                    PrimitiveStyle::with_fill(BinaryColor::On)
                } else {
                    PrimitiveStyle::with_stroke(BinaryColor::On, 1)
                };
                rect.into_styled(style)
                    .draw(target)
                    .map_err(|e| anyhow!("draw rect: {:?}", e))?;
            }
            Element::Line { start, end } => {
                Line::new(*start, *end)
                    .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1))
                    .draw(target)
                    .map_err(|e| anyhow!("draw line: {:?}", e))?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

/// Owns the framebuffer + (eventually) the EPD driver, and decides
/// full-vs-partial refresh on each `present`.
pub struct Panel {
    framebuffer: Display2in13,
    backend: Backend,
    last_hash: Option<u64>,
    partials_since_full: u32,
}

enum Backend {
    /// No panel attached: render onto the framebuffer (so the renderer
    /// is exercised end-to-end), log what would have been pushed, but
    /// don't touch SPI.
    Stub,
    // Hardware { epd, spi, delay } variant to be added once the panel
    // is wired. Field types depend on the SPI device wrapper we hand
    // in, so it's left out until we have one to constrain it.
}

impl Panel {
    /// Construct a panel in stub mode — no hardware required. The
    /// framebuffer + renderer still run, so layout iteration is
    /// faithful; only the EPD flush is skipped.
    pub fn stub() -> Self {
        let mut framebuffer = Display2in13::default();
        framebuffer.set_rotation(DisplayRotation::Rotate90);
        Self {
            framebuffer,
            backend: Backend::Stub,
            last_hash: None,
            partials_since_full: 0,
        }
    }

    /// Render `scene` and (in hardware mode) push to the panel.
    ///
    /// - Identical content → no-op (returns `Ok(())` without redrawing).
    /// - First present after boot, every `FULL_REFRESH_EVERY` partials,
    ///   or `scene.force_full()` → full refresh.
    /// - Otherwise → partial refresh.
    pub fn present(&mut self, scene: &Scene) -> Result<()> {
        let hash = scene.content_hash();
        if Some(hash) == self.last_hash && !scene.force_full {
            tracing::trace!("display: scene unchanged, skipping present");
            return Ok(());
        }

        let force_full = scene.force_full
            || self.last_hash.is_none()
            || self.partials_since_full >= FULL_REFRESH_EVERY;

        self.framebuffer
            .clear(BinaryColor::Off)
            .map_err(|e| anyhow!("clear framebuffer: {:?}", e))?;
        render_scene(&mut self.framebuffer, scene)?;

        match &mut self.backend {
            Backend::Stub => {
                let on_pixels = count_on_pixels(&self.framebuffer);
                tracing::info!(
                    elements = scene.elements.len(),
                    on_pixels,
                    full_refresh = force_full,
                    scene_hash = format_args!("{:016x}", hash),
                    "display: stub present (no panel attached, render-only)",
                );
            }
        }

        self.last_hash = Some(hash);
        if force_full {
            self.partials_since_full = 0;
        } else {
            self.partials_since_full += 1;
        }
        Ok(())
    }
}

/// Cheap "did the renderer actually draw something" sanity counter.
/// Iterating the framebuffer is ~3.8KB of work per present in stub
/// mode; meaningful while we have no eyes on real glass. Drop once
/// the hardware path lands.
fn count_on_pixels(fb: &Display2in13) -> u32 {
    fb.buffer().iter().map(|b| b.count_ones()).sum()
}

// ---------------------------------------------------------------------------
// Demo scene
// ---------------------------------------------------------------------------

/// Minimal info-card scene used at boot: device version + IP, with a
/// rule under the title. Useful both as a smoke test for the renderer
/// and as a starting template for richer scenes (project #1 from
/// `docs/eink-plan.md`).
pub fn boot_info_scene(version: &str, ip: &str) -> Scene {
    Scene::new()
        .text("esp32-blinky", Point::new(2, 0), Font::Medium)
        .line(Point::new(0, 18), Point::new(WIDTH as i32 - 1, 18))
        .text(format!("ip: {}", ip), Point::new(2, 24), Font::Small)
        .text(format!("fw: {}", version), Point::new(2, 38), Font::Small)
}
