//! Regenerates the brand files the app, its platforms and its bundle use
//! from the editable sources in `assets/brand/src/` (from the BroScribe
//! design system). One drawing per job, in each system's own format:
//!
//! - `assets/brand/*.svg`: each source as plain paths, the mascot's `Bro_`
//!   set in the bundled Inter, so GPUI's renderer never depends on an
//!   installed font;
//! - the app icon, after the brand guide's size rule (the waveform tile
//!   below 64 pixels, the hero tile from 64 up):
//!   - `AppIcon.iconset/`: macOS, on Apple's icon grid (824 of 1024);
//!   - `windows/BroScribe.ico` and `windows/BroScribe.res`: Windows, the
//!     same images as an icon file and as the executable's icon resource
//!     (ID 1, which GPUI loads for its windows);
//!   - `linux/window-icon.png`: the X11 window icon;
//! - the status item, the waveform in three states (idle, recording with a
//!   dot, paused with two bars), always one ink:
//!   - `menubar/`: macOS template images (24×18 points, 1x and 2x);
//!   - `windows/tray-<state>-<taskbar>.ico`: the notification area, in the
//!     theme's dark ink for a light taskbar and its light ink for a dark
//!     one;
//!   - `linux/broscribe-<state>-symbolic.svg`: StatusNotifierItem icons
//!     that GTK and KDE panels paint in their own text colour (filled
//!     shapes in `currentColor` with KDE's colour-scheme class), the Linux
//!     counterpart of a template image;
//!   - `linux/tray-<state>-<size>.png`: pixmaps for hosts that cannot load
//!     them, light ink inside a dark ring, which read on light and dark
//!     panels.
//!
//! Run it after changing a source and commit the results:
//! `cargo run -p broscribe-app --example render_brand`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use resvg::{tiny_skia, usvg};

type Error = Box<dyn std::error::Error>;

/// macOS iconset sizes (points), each at 1x and 2x.
const ICONSET_SIZES: [u32; 5] = [16, 32, 128, 256, 512];
/// The share of the macOS icon canvas the tile covers (Apple's grid).
const ICONSET_SHAPE: f32 = 824.0 / 1024.0;
/// From this many pixels up an app icon shows the face; below it, the
/// waveform tile.
const FACE_FROM: u32 = 64;
/// Windows icon sizes: 100–300 % small icons, Explorer's views, and 256.
const WINDOWS_ICON_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
/// Notification-area icon sizes (16 at 100 % to 48 at 300 %).
const WINDOWS_TRAY_SIZES: [u32; 6] = [16, 20, 24, 32, 40, 48];
/// StatusNotifierItem pixmap sizes; the host scales the closest.
const LINUX_TRAY_SIZES: [u32; 4] = [22, 32, 44, 64];
const LINUX_WINDOW_ICON: u32 = 128;
/// The menu bar's glyph height in points.
const MENUBAR_HEIGHT: u32 = 18;
/// The status item's states, as the `menubar-<state>.svg` sources name them.
const STATES: [&str; 3] = ["idle", "recording", "paused"];
/// The theme's `foreground` in light and in dark (assets/themes).
const DARK_INK: [u8; 3] = [0x1c, 0x1c, 0x1a];
const LIGHT_INK: [u8; 3] = [0xed, 0xed, 0xea];

fn main() -> Result<(), Error> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");
    let brand = root.join("brand");
    let options = options(&root.join("fonts"))?;

    let mut sources: Vec<PathBuf> = std::fs::read_dir(brand.join("src"))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()?;
    sources.retain(|path| path.extension().is_some_and(|ext| ext == "svg"));
    sources.sort();

    let mut trees = BTreeMap::new();
    for source in &sources {
        let tree = usvg::Tree::from_data(&std::fs::read(source)?, &options)?;
        let name = source.file_name().ok_or("source without a name")?;
        std::fs::write(
            brand.join(name),
            tree.to_string(&usvg::WriteOptions::default()),
        )?;
        let stem = source
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or("source without a name")?;
        trees.insert(stem.to_owned(), tree);
    }
    let tree = |stem: &str| trees.get(stem).ok_or(format!("missing src/{stem}.svg"));

    let tiles = Tiles {
        glyph: tree("tile-glyph")?,
        hero: tree("tile-hero")?,
    };
    render_iconset(&tiles, &brand.join("AppIcon.iconset"))?;
    render_windows_app(&tiles, &brand.join("windows"))?;
    std::fs::create_dir_all(brand.join("linux"))?;
    tiles
        .render(LINUX_WINDOW_ICON, false)
        .save_png(brand.join("linux/window-icon.png"))?;
    let glyphs = STATES
        .into_iter()
        .map(|state| Ok((state, tree(&format!("menubar-{state}"))?)))
        .collect::<Result<Vec<_>, Error>>()?;
    // One scale for every state, so the bars keep their size when the
    // state changes: the largest drawing fills the square.
    let extent = glyphs
        .iter()
        .map(|(_, glyph)| {
            let bounds = glyph.root().abs_stroke_bounding_box();
            bounds.width().max(bounds.height())
        })
        .fold(0.0, f32::max);
    for (state, glyph) in glyphs {
        render_menubar(glyph, state, &brand.join("menubar"))?;
        render_windows_tray(glyph, extent, state, &brand.join("windows"))?;
        render_linux_tray(glyph, extent, state, &brand.join("linux"))?;
        render_linux_symbolic(glyph, extent, state, &brand.join("linux"))?;
    }
    println!(
        "rendered {} sources into {}",
        sources.len(),
        brand.display()
    );
    Ok(())
}

/// usvg options whose font database holds only the bundled fonts, so the
/// output is the same on every machine.
fn options(fonts: &Path) -> Result<usvg::Options<'static>, Error> {
    let mut db = usvg::fontdb::Database::new();
    for entry in std::fs::read_dir(fonts)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "ttf") {
            db.load_font_file(&path)?;
        }
    }
    db.set_sans_serif_family("Inter");
    Ok(usvg::Options {
        font_family: "Inter".into(),
        fontdb: Arc::new(db),
        ..Default::default()
    })
}

/// The two app-icon drawings.
struct Tiles<'a> {
    glyph: &'a usvg::Tree,
    hero: &'a usvg::Tree,
}

impl Tiles<'_> {
    fn drawing(&self, pixels: u32) -> &usvg::Tree {
        if pixels < FACE_FROM {
            self.glyph
        } else {
            self.hero
        }
    }

    fn render(&self, pixels: u32, macos: bool) -> tiny_skia::Pixmap {
        let tree = self.drawing(pixels);
        let side = pixels as f32;
        let source = tree.size();
        let transform = if macos {
            // Both sources already inset the visible squircle within their
            // canvas. Fit the drawing's bounds to Apple's grid, only once.
            let bounds = tree.root().abs_stroke_bounding_box();
            square(
                tree,
                bounds.width().max(bounds.height()),
                side * ICONSET_SHAPE,
            )
            .post_translate(
                side * (1. - ICONSET_SHAPE) / 2.,
                side * (1. - ICONSET_SHAPE) / 2.,
            )
        } else {
            tiny_skia::Transform::from_scale(side / source.width(), side / source.height())
        };
        render(tree, pixels, pixels, transform)
    }
}

fn render(
    tree: &usvg::Tree,
    width: u32,
    height: u32,
    transform: tiny_skia::Transform,
) -> tiny_skia::Pixmap {
    let mut pixmap = tiny_skia::Pixmap::new(width, height).expect("a non-empty image");
    resvg::render(tree, transform, &mut pixmap.as_mut());
    pixmap
}

fn render_iconset(tiles: &Tiles<'_>, out: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(out)?;
    for size in ICONSET_SIZES {
        for scale in [1, 2] {
            let name = if scale == 1 {
                format!("icon_{size}x{size}.png")
            } else {
                format!("icon_{size}x{size}@2x.png")
            };
            tiles.render(size * scale, true).save_png(out.join(name))?;
        }
    }
    Ok(())
}

fn render_menubar(tree: &usvg::Tree, state: &str, out: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(out)?;
    let source = tree.size();
    for scale in [1, 2] {
        let height = MENUBAR_HEIGHT * scale;
        let factor = height as f32 / source.height();
        let width = (source.width() * factor).round() as u32;
        let name = if scale == 1 {
            format!("{state}.png")
        } else {
            format!("{state}@2x.png")
        };
        render(
            tree,
            width,
            height,
            tiny_skia::Transform::from_scale(factor, factor),
        )
        .save_png(out.join(name))?;
    }
    Ok(())
}

/// Draws the status glyph centred on its own drawing in a `size` square,
/// `extent` source units across it. (The macOS menu-bar canvases share a
/// 24×18-point footprint with the idle waveform centred, so neighbouring items
/// keep one width; a square icon centres what is drawn instead.)
fn square(tree: &usvg::Tree, extent: f32, size: f32) -> tiny_skia::Transform {
    let bounds = tree.root().abs_stroke_bounding_box();
    let factor = size / extent;
    tiny_skia::Transform::from_row(
        factor,
        0.0,
        0.0,
        factor,
        (size - bounds.width() * factor) / 2.0 - bounds.x() * factor,
        (size - bounds.height() * factor) / 2.0 - bounds.y() * factor,
    )
}

/// The square status glyph as coverage: 0 outside the shape, 255 inside.
fn glyph_coverage(tree: &usvg::Tree, extent: f32, size: u32) -> Vec<u8> {
    let transform = square(tree, extent, size as f32);
    render(tree, size, size, transform)
        .pixels()
        .iter()
        .map(|pixel| pixel.alpha())
        .collect()
}

/// Straight (not premultiplied) RGBA pixels.
type Rgba = Vec<[u8; 4]>;

fn straight(pixmap: &tiny_skia::Pixmap) -> Rgba {
    pixmap
        .pixels()
        .iter()
        .map(|pixel| {
            let colour = pixel.demultiply();
            [colour.red(), colour.green(), colour.blue(), colour.alpha()]
        })
        .collect()
}

fn ink(coverage: &[u8], [r, g, b]: [u8; 3]) -> Rgba {
    coverage.iter().map(|&a| [r, g, b, a]).collect()
}

fn render_windows_app(tiles: &Tiles<'_>, out: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(out)?;
    let images: Vec<(u32, Vec<u8>)> = WINDOWS_ICON_SIZES
        .into_iter()
        .map(|size| {
            let pixmap = tiles.render(size, false);
            // 256 is stored as PNG, smaller sizes as 32-bit bitmaps, as
            // Windows' own icons are.
            let data = if size >= 256 {
                pixmap.encode_png()?
            } else {
                dib(size, &straight(&pixmap))
            };
            Ok((size, data))
        })
        .collect::<Result<_, Error>>()?;
    std::fs::write(out.join("BroScribe.ico"), ico(&images))?;
    std::fs::write(out.join("BroScribe.res"), res(&images))?;
    Ok(())
}

fn render_windows_tray(
    tree: &usvg::Tree,
    extent: f32,
    state: &str,
    out: &Path,
) -> Result<(), Error> {
    std::fs::create_dir_all(out)?;
    for (taskbar, colour) in [("light", DARK_INK), ("dark", LIGHT_INK)] {
        let images: Vec<(u32, Vec<u8>)> = WINDOWS_TRAY_SIZES
            .into_iter()
            .map(|size| {
                let coverage = glyph_coverage(tree, extent, size);
                (size, dib(size, &ink(&coverage, colour)))
            })
            .collect();
        std::fs::write(
            out.join(format!("tray-{state}-{taskbar}.ico")),
            ico(&images),
        )?;
    }
    Ok(())
}

fn render_linux_tray(tree: &usvg::Tree, extent: f32, state: &str, out: &Path) -> Result<(), Error> {
    for size in LINUX_TRAY_SIZES {
        // A one-pixel ring at 22 px, growing with the size; the glyph sits
        // inside it, so the ring never touches the edge.
        let ring = (size as f32 / 22.0).round().max(1.0) as i32;
        let inner = size - 2 * ring as u32;
        let coverage = pad(&glyph_coverage(tree, extent, inner), inner, ring as u32);
        let pixels = ringed(&coverage, size, ring, LIGHT_INK, DARK_INK);
        let premultiplied: Vec<u8> = pixels
            .iter()
            .flat_map(|&[r, g, b, a]| {
                let colour = tiny_skia::ColorU8::from_rgba(r, g, b, a).premultiply();
                [colour.red(), colour.green(), colour.blue(), colour.alpha()]
            })
            .collect();
        tiny_skia::Pixmap::from_vec(
            premultiplied,
            tiny_skia::IntSize::from_wh(size, size).ok_or("empty image")?,
        )
        .ok_or("bad pixmap")?
        .save_png(out.join(format!("tray-{state}-{size}.png")))?;
    }
    Ok(())
}

/// The size symbolic icons are drawn at (GTK's nominal symbolic size).
const SYMBOLIC_SIZE: f32 = 16.0;

/// The square status glyph as one filled path (strokes outlined, since GTK
/// recolours fills only) in `currentColor`, with the stylesheet KDE uses to
/// recolour `ColorScheme-Text`.
fn render_linux_symbolic(
    tree: &usvg::Tree,
    extent: f32,
    state: &str,
    out: &Path,
) -> Result<(), Error> {
    let transform = square(tree, extent, SYMBOLIC_SIZE);
    let mut data = String::new();
    outline(tree.root(), transform, &mut data);
    if data.is_empty() {
        return Err(format!("menubar-{state}.svg draws nothing").into());
    }
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"16\" height=\"16\" viewBox=\"0 0 16 16\">\
<style id=\"current-color-scheme\">.ColorScheme-Text{{color:#232629}}</style>\
<path class=\"ColorScheme-Text\" fill=\"currentColor\" d=\"{data}\"/></svg>\n"
    );
    std::fs::write(out.join(format!("broscribe-{state}-symbolic.svg")), svg)?;
    Ok(())
}

/// Appends every path under `group` to `data`, strokes as their outlines,
/// in `transform`'s coordinates.
fn outline(group: &usvg::Group, transform: tiny_skia::Transform, data: &mut String) {
    for node in group.children() {
        match node {
            usvg::Node::Group(group) => outline(group, transform, data),
            usvg::Node::Path(path) => {
                let to_icon = path.abs_transform().post_concat(transform);
                if path.fill().is_some() {
                    append(path.data().clone().transform(to_icon), data);
                }
                if let Some(stroke) = path.stroke() {
                    let outlined = path.data().stroke(&stroke.to_tiny_skia(), 4.0);
                    append(outlined.and_then(|o| o.transform(to_icon)), data);
                }
            }
            _ => {}
        }
    }
}

fn append(path: Option<tiny_skia::Path>, data: &mut String) {
    use std::fmt::Write as _;
    use tiny_skia::PathSegment;
    let Some(path) = path else { return };
    // Three decimals, and never "-0".
    let n = |v: f32| (v * 1000.0).round() / 1000.0 + 0.0;
    for segment in path.segments() {
        let _ = match segment {
            PathSegment::MoveTo(p) => write!(data, "M{} {}", n(p.x), n(p.y)),
            PathSegment::LineTo(p) => write!(data, "L{} {}", n(p.x), n(p.y)),
            PathSegment::QuadTo(a, p) => {
                write!(data, "Q{} {} {} {}", n(a.x), n(a.y), n(p.x), n(p.y))
            }
            PathSegment::CubicTo(a, b, p) => write!(
                data,
                "C{} {} {} {} {} {}",
                n(a.x),
                n(a.y),
                n(b.x),
                n(b.y),
                n(p.x),
                n(p.y)
            ),
            PathSegment::Close => write!(data, "Z"),
        };
    }
}

/// `coverage` (`side` square) with `by` empty pixels on every side.
fn pad(coverage: &[u8], side: u32, by: u32) -> Vec<u8> {
    let (side, by) = (side as usize, by as usize);
    let outer = side + 2 * by;
    let mut out = vec![0; outer * outer];
    for (y, row) in coverage.chunks(side).enumerate() {
        let start = (y + by) * outer + by;
        out[start..start + side].copy_from_slice(row);
    }
    out
}

/// `coverage` filled with `fill` inside a `ring`-pixel band of `outline`.
fn ringed(coverage: &[u8], size: u32, ring: i32, fill: [u8; 3], outline: [u8; 3]) -> Rgba {
    let size = size as i32;
    let at = |x: i32, y: i32| {
        if (0..size).contains(&x) && (0..size).contains(&y) {
            f32::from(coverage[(y * size + x) as usize]) / 255.0
        } else {
            0.0
        }
    };
    let mut pixels = Vec::with_capacity(coverage.len());
    for y in 0..size {
        for x in 0..size {
            let shape = at(x, y);
            let mut band: f32 = 0.0;
            for dy in -ring..=ring {
                for dx in -ring..=ring {
                    if dx * dx + dy * dy <= ring * ring + ring {
                        band = band.max(at(x + dx, y + dy));
                    }
                }
            }
            let behind = band * (1.0 - shape);
            let alpha = shape + behind;
            let channel = |i: usize| {
                if alpha == 0.0 {
                    0
                } else {
                    ((f32::from(fill[i]) * shape + f32::from(outline[i]) * behind) / alpha).round()
                        as u8
                }
            };
            pixels.push([
                channel(0),
                channel(1),
                channel(2),
                (alpha * 255.0).round() as u8,
            ]);
        }
    }
    pixels
}

/// A 32-bit icon image: BITMAPINFOHEADER (height doubled for the mask),
/// bottom-up BGRA, then the 1-bit AND mask (set where fully transparent).
fn dib(size: u32, pixels: &[[u8; 4]]) -> Vec<u8> {
    let side = size as usize;
    let mask_row = side.div_ceil(32) * 4;
    let mut out = Vec::with_capacity(40 + side * side * 4 + mask_row * side);
    out.extend(40u32.to_le_bytes());
    out.extend((size as i32).to_le_bytes());
    out.extend((2 * size as i32).to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // BI_RGB
    out.extend(((side * side * 4 + mask_row * side) as u32).to_le_bytes());
    out.extend([0u8; 16]);
    for row in pixels.chunks(side).rev() {
        for &[r, g, b, a] in row {
            out.extend([b, g, r, a]);
        }
    }
    for row in pixels.chunks(side).rev() {
        let mut mask = vec![0u8; mask_row];
        for (x, pixel) in row.iter().enumerate() {
            if pixel[3] == 0 {
                mask[x / 8] |= 0x80 >> (x % 8);
            }
        }
        out.extend(mask);
    }
    out
}

/// The width or height byte of an icon directory entry (0 means 256).
fn dimension(size: u32) -> u8 {
    if size >= 256 { 0 } else { size as u8 }
}

/// An `.ico` file: ICONDIR, one ICONDIRENTRY per image, then the images.
fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(0u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend((images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len();
    for (size, data) in images {
        out.extend([dimension(*size), dimension(*size), 0, 0]);
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out.extend((offset as u32).to_le_bytes());
        offset += data.len();
    }
    for (_, data) in images {
        out.extend(data);
    }
    out
}

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
/// en-US, the language rc.exe gives resources by default.
const LANGUAGE: u16 = 0x0409;

/// A compiled resource file (`.res`) holding the icon as RT_ICON images 1…n
/// and the RT_GROUP_ICON 1 that lists them. MSVC's linker takes it as an
/// input file; GPUI loads icon 1 from the executable for its windows.
fn res(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    // Every .res file starts with an empty entry.
    resource(&mut out, 0, 0, 0, 0, &[]);
    for (index, (_, data)) in images.iter().enumerate() {
        resource(&mut out, RT_ICON, index as u16 + 1, 0x1010, LANGUAGE, data);
    }
    let mut group = Vec::new();
    group.extend(0u16.to_le_bytes());
    group.extend(1u16.to_le_bytes());
    group.extend((images.len() as u16).to_le_bytes());
    for (index, (size, data)) in images.iter().enumerate() {
        group.extend([dimension(*size), dimension(*size), 0, 0]);
        group.extend(1u16.to_le_bytes());
        group.extend(32u16.to_le_bytes());
        group.extend((data.len() as u32).to_le_bytes());
        group.extend((index as u16 + 1).to_le_bytes());
    }
    resource(&mut out, RT_GROUP_ICON, 1, 0x1030, LANGUAGE, &group);
    out
}

/// One RESOURCEHEADER (ordinal type and name) and its data, padded to four
/// bytes.
fn resource(out: &mut Vec<u8>, kind: u16, name: u16, flags: u16, language: u16, data: &[u8]) {
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(32u32.to_le_bytes());
    out.extend([0xff, 0xff]);
    out.extend(kind.to_le_bytes());
    out.extend([0xff, 0xff]);
    out.extend(name.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // DataVersion
    out.extend(flags.to_le_bytes());
    out.extend(language.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // Version
    out.extend(0u32.to_le_bytes()); // Characteristics
    out.extend(data);
    out.resize(out.len().next_multiple_of(4), 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_macos_icons_match_the_grid_and_small_sizes_use_the_waveform() {
        let assets = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");
        let brand = assets.join("brand");
        let options = options(&assets.join("fonts")).unwrap();
        let load = |file| {
            usvg::Tree::from_data(&std::fs::read(brand.join(file)).unwrap(), &options).unwrap()
        };
        let glyph = load("src/tile-glyph.svg");
        let hero = load("src/tile-hero.svg");
        let tiles = Tiles {
            glyph: &glyph,
            hero: &hero,
        };
        assert!(std::ptr::eq(tiles.drawing(32), &glyph));
        assert!(std::ptr::eq(tiles.drawing(63), &glyph));
        assert!(std::ptr::eq(tiles.drawing(64), &hero));

        let icon =
            |name| tiny_skia::Pixmap::load_png(brand.join("AppIcon.iconset").join(name)).unwrap();
        assert_eq!(icon("icon_32x32.png"), tiles.render(32, true));
        assert_eq!(icon("icon_32x32@2x.png"), tiles.render(64, true));
        let large = icon("icon_512x512@2x.png");
        assert_eq!(large, tiles.render(1024, true));
        let drawn = (0..1024)
            .filter(|x| large.pixel(*x, 512).unwrap().alpha() != 0)
            .collect::<Vec<_>>();
        assert_eq!(
            (drawn.first(), drawn.last(), drawn.len()),
            (Some(&100), Some(&923), 824)
        );
    }
}
