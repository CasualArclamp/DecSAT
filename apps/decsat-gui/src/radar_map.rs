//! A map under the radar scope: OpenStreetMap's raster tiles, drawn in the
//! scope's own frame — north up, NM east and north of the radar — so the
//! targets sit on the ground they fly over. "Dark" is the same tiles with
//! their lightness turned over here (CARTO's dark basemap now wants an API
//! key).
//!
//! **Where the radar is** comes from the feed when it says (ASTERIX
//! I034/120), else from the user, once, for the VFO: saved in their own
//! settings (`radar_site.<VFO name>` in `prefs.txt`), never in the project.
//!
//! **Projection.** The feed's x/y are taken as range and bearing from the
//! radar (azimuthal equidistant on a sphere, R = 3440.065 NM): at 250 NM
//! that is within ~0.2 NM of a stereographic system plane. Each tile's
//! corners and a 4 × 4 grid inside it are carried into the scope's frame,
//! so the Web Mercator tiles bend to fit.
//!
//! **Tiles** are fetched one at a time on a worker thread with the
//! system's `curl` (Windows 10/11 has it; no TLS stack in DecSAT, whose
//! licence would sit awkwardly with ring's), cached on disk for good
//! (`<settings>/DecSAT/tiles/osm/z/x/y.png`), and credited on the map, as
//! the OpenStreetMap tile policy asks: a named User-Agent, a cache, no bulk
//! downloading.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, ColorImage, FontId, Mesh, Pos2, Rect, TextureHandle, TextureOptions,
    pos2, vec2,
};

/// Earth's radius, NM (mean).
const R_NM: f64 = 3440.065;
/// Subdivisions per tile side when bending a tile into the scope's frame.
const GRID: usize = 4;
/// Most tiles drawn at once (a zoom level coarser is used beyond this).
const MAX_TILES: usize = 80;
/// Tiles kept as textures; beyond this those not on screen are dropped.
const KEEP_TILES: usize = 300;
/// A failed fetch is tried again after this.
const RETRY: Duration = Duration::from_secs(60);

/// Where a radar is (degrees, WGS-84).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

/// Latitude/longitude → NM east and north of `site` (azimuthal
/// equidistant).
pub fn to_nm(site: LatLon, p: LatLon) -> (f64, f64) {
    let (p0, l0) = (site.lat.to_radians(), site.lon.to_radians());
    let (p1, l1) = (p.lat.to_radians(), p.lon.to_radians());
    let dl = l1 - l0;
    let cos_c = (p0.sin() * p1.sin() + p0.cos() * p1.cos() * dl.cos()).clamp(-1.0, 1.0);
    let c = cos_c.acos();
    let k = if c < 1e-12 { 1.0 } else { c / c.sin() };
    (
        R_NM * k * p1.cos() * dl.sin(),
        R_NM * k * (p0.cos() * p1.sin() - p0.sin() * p1.cos() * dl.cos()),
    )
}

/// NM east and north of `site` → latitude/longitude (the inverse).
pub fn from_nm(site: LatLon, x: f64, y: f64) -> LatLon {
    let rho = x.hypot(y);
    if rho < 1e-9 {
        return site;
    }
    let (p0, l0) = (site.lat.to_radians(), site.lon.to_radians());
    let c = rho / R_NM;
    let lat = (c.cos() * p0.sin() + y * c.sin() * p0.cos() / rho).asin();
    let lon = l0 + (x * c.sin()).atan2(rho * p0.cos() * c.cos() - y * p0.sin() * c.sin());
    LatLon {
        lat: lat.to_degrees(),
        lon: lon.to_degrees(),
    }
}

/// Web Mercator tile coordinates (fractional) of a point at zoom `z`.
fn tile_of(p: LatLon, z: u8) -> (f64, f64) {
    let n = f64::from(1u32 << z);
    let lat = p.lat.clamp(-85.05, 85.05).to_radians();
    (
        (p.lon + 180.0) / 360.0 * n,
        (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n,
    )
}

/// The point at tile coordinates (fractional) at zoom `z`.
fn point_of(tx: f64, ty: f64, z: u8) -> LatLon {
    let n = f64::from(1u32 << z);
    LatLon {
        lat: (std::f64::consts::PI * (1.0 - 2.0 * ty / n))
            .sinh()
            .atan()
            .to_degrees(),
        lon: tx / n * 360.0 - 180.0,
    }
}

/// The map's look.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Style {
    /// OpenStreetMap's tiles with their lightness turned over: dark land,
    /// dark teal sea, light lettering — quiet under the targets.
    #[default]
    Dark,
    /// OpenStreetMap's own tiles.
    Osm,
}

impl Style {
    pub const ALL: [Style; 2] = [Style::Dark, Style::Osm];

    pub fn label(self) -> &'static str {
        match self {
            Style::Dark => "dark",
            Style::Osm => "OpenStreetMap",
        }
    }

    /// Both styles are drawn from the one set of tiles.
    fn dir(self) -> &'static str {
        "osm"
    }

    fn url(self, z: u8, x: u32, y: u32) -> String {
        format!("https://tile.openstreetmap.org/{z}/{x}/{y}.png")
    }

    pub fn credit(self) -> &'static str {
        "© OpenStreetMap contributors"
    }
}

type Key = (Style, u8, u32, u32);

enum Tile {
    Pending,
    Ready(TextureHandle),
    Failed(Instant),
}

/// The tiles, their fetcher, and what went wrong last.
#[derive(Default)]
pub struct MapLayer {
    tiles: HashMap<Key, Tile>,
    requests: Option<Sender<Key>>,
    results: Option<Receiver<(Key, Result<ColorImage, String>)>>,
    pub error: Option<String>,
}

/// Where tiles are kept: beside the settings.
fn cache_root() -> Option<PathBuf> {
    Some(crate::prefs::settings_dir()?.join("tiles"))
}

/// A tile from the disk cache, else fetched into it; decoded.
fn fetch(key: Key) -> Result<ColorImage, String> {
    let (style, z, x, y) = key;
    let path = cache_root()
        .ok_or("no settings folder")?
        .join(style.dir())
        .join(z.to_string())
        .join(x.to_string())
        .join(format!("{y}.png"));
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let part = path.with_extension("part");
        let mut cmd = std::process::Command::new("curl");
        cmd.args(["-sfL", "--max-time", "20", "-A"])
            .arg(concat!(
                "DecSAT/",
                env!("CARGO_PKG_VERSION"),
                " (radar scope map; +https://github.com/CasualArclamp/DecSAT)"
            ))
            .arg("-o")
            .arg(&part)
            .arg(style.url(z, x, y));
        #[cfg(windows)]
        {
            // Rust note: `CommandExt` adds Windows-only options to
            // `Command`; CREATE_NO_WINDOW keeps a console from flashing up
            // from the windowed app.
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let ok = cmd.status().map_err(|e| format!("curl: {e}"))?.success();
        if !ok {
            let _ = std::fs::remove_file(&part);
            return Err("tile download failed (offline?)".into());
        }
        std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
    }
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let img = image::load_from_memory(&bytes).map_err(|e| {
        // A bad file must not stick: fetch it again next time.
        let _ = std::fs::remove_file(&path);
        e.to_string()
    })?;
    let mut rgba = img.to_rgba8();
    if style == Style::Dark {
        for px in rgba.pixels_mut() {
            px.0 = darken(px.0);
        }
    }
    Ok(ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    ))
}

/// A pixel with its HSL lightness L turned to 1 − L, hue and saturation
/// kept: the chroma max − min is the same at L and 1 − L, so every channel
/// moves by the same (1 − L) − L = 1 − (max + min).
fn darken([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    let (hi, lo) = (r.max(g).max(b), r.min(g).min(b));
    let shift = 255 - i32::from(hi) - i32::from(lo);
    let f = |c: u8| (i32::from(c) + shift).clamp(0, 255) as u8;
    [f(r), f(g), f(b), a]
}

/// What [`MapLayer::draw`] needs of the scope.
pub struct Frame {
    pub rect: Rect,
    pub site: LatLon,
    /// Screen pixel of a point NM east and north of the radar, and back.
    pub to_screen: Box<dyn Fn(f64, f64) -> Pos2>,
    pub from_screen: Box<dyn Fn(Pos2) -> (f64, f64)>,
    pub px_per_nm: f32,
}

impl MapLayer {
    /// The fetching thread, started on first use.
    fn worker(&mut self) -> &Sender<Key> {
        if self.requests.is_none() {
            let (req_tx, req_rx) = channel::<Key>();
            let (res_tx, res_rx) = channel();
            std::thread::Builder::new()
                .name("map tiles".into())
                .spawn(move || {
                    while let Ok(key) = req_rx.recv() {
                        if res_tx.send((key, fetch(key))).is_err() {
                            break;
                        }
                    }
                })
                .expect("spawn the map tile thread");
            self.requests = Some(req_tx);
            self.results = Some(res_rx);
        }
        self.requests.as_ref().expect("just made")
    }

    /// Draw the map under the scope, `brightness` 0–1.
    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        f: &Frame,
        style: Style,
        brightness: f32,
    ) {
        // Tiles that have come in become textures.
        if let Some(rx) = &self.results {
            for (key, r) in rx.try_iter() {
                match r {
                    Ok(img) => {
                        let name = format!("map/{}/{}/{}/{}", key.0.dir(), key.1, key.2, key.3);
                        let tex = ctx.load_texture(name, img, TextureOptions::LINEAR);
                        self.tiles.insert(key, Tile::Ready(tex));
                        self.error = None;
                    }
                    Err(e) => {
                        self.tiles.insert(key, Tile::Failed(Instant::now()));
                        self.error = Some(e);
                    }
                }
            }
        }
        // The zoom whose pixels are about the screen's.
        let coslat = f.site.lat.to_radians().cos().max(0.05);
        let n_wanted = f64::from(f.px_per_nm) * 60.0 * coslat * 360.0 / 256.0;
        let mut z = n_wanted.log2().round().clamp(2.0, 13.0) as u8;
        // The view's extent, from points round its edge.
        let r = f.rect;
        let edge: Vec<Pos2> = (0..=8)
            .flat_map(|i| {
                let t = i as f32 / 8.0;
                [
                    pos2(r.left() + t * r.width(), r.top()),
                    pos2(r.left() + t * r.width(), r.bottom()),
                    pos2(r.left(), r.top() + t * r.height()),
                    pos2(r.right(), r.top() + t * r.height()),
                ]
            })
            .collect();
        let (x0, x1, y0, y1) = loop {
            let mut b = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
            for &p in &edge {
                let (x, y) = (f.from_screen)(p);
                let (tx, ty) = tile_of(from_nm(f.site, x, y), z);
                b = (b.0.min(tx), b.1.max(tx), b.2.min(ty), b.3.max(ty));
            }
            let n = f64::from(1u32 << z);
            let (x0, x1) = (b.0.floor().max(0.0) as u32, b.1.floor().min(n - 1.0) as u32);
            let (y0, y1) = (b.2.floor().max(0.0) as u32, b.3.floor().min(n - 1.0) as u32);
            let count = (x1 - x0 + 1) as usize * (y1 - y0 + 1) as usize;
            if count <= MAX_TILES || z <= 2 {
                break (x0, x1, y0, y1);
            }
            z -= 1;
        };
        // Nearest the centre first.
        let (cx, cy) = (f64::from(x0 + x1) / 2.0, f64::from(y0 + y1) / 2.0);
        let mut want: Vec<(u32, u32)> = (x0..=x1)
            .flat_map(|x| (y0..=y1).map(move |y| (x, y)))
            .collect();
        want.sort_by(|a, b| {
            let d = |t: &(u32, u32)| (f64::from(t.0) - cx).hypot(f64::from(t.1) - cy);
            d(a).total_cmp(&d(b))
        });
        let tint = Color32::from_gray((brightness.clamp(0.0, 1.0) * 255.0) as u8);
        for &(x, y) in &want {
            let key = (style, z, x, y);
            match self.tiles.get(&key) {
                Some(Tile::Ready(tex)) => {
                    painter.add(egui::Shape::mesh(tile_mesh(f, tex.id(), z, x, y, tint)));
                }
                Some(Tile::Pending) => {}
                Some(Tile::Failed(at)) if at.elapsed() < RETRY => {}
                _ => {
                    self.tiles.insert(key, Tile::Pending);
                    let _ = self.worker().send(key);
                }
            }
        }
        // Let go of textures far from the view.
        if self.tiles.len() > KEEP_TILES {
            self.tiles.retain(|k, t| {
                matches!(t, Tile::Pending)
                    || (k.0 == style && k.1 == z && want.contains(&(k.2, k.3)))
            });
        }
        painter.text(
            r.right_bottom() + vec2(-6.0, -6.0),
            Align2::RIGHT_BOTTOM,
            style.credit(),
            FontId::proportional(10.0),
            Color32::from_gray(140),
        );
    }
}

/// Tile (z, x, y) bent into the scope's frame: a grid of vertices.
fn tile_mesh(f: &Frame, tex: egui::TextureId, z: u8, x: u32, y: u32, tint: Color32) -> Mesh {
    let mut m = Mesh::with_texture(tex);
    for j in 0..=GRID {
        for i in 0..=GRID {
            let (u, v) = (i as f64 / GRID as f64, j as f64 / GRID as f64);
            let p = point_of(f64::from(x) + u, f64::from(y) + v, z);
            let (ex, ny) = to_nm(f.site, p);
            m.vertices.push(egui::epaint::Vertex {
                pos: (f.to_screen)(ex, ny),
                uv: pos2(u as f32, v as f32),
                color: tint,
            });
        }
    }
    let w = (GRID + 1) as u32;
    for j in 0..GRID as u32 {
        for i in 0..GRID as u32 {
            let a = j * w + i;
            m.add_triangle(a, a + 1, a + w);
            m.add_triangle(a + 1, a + w + 1, a + w);
        }
    }
    m
}

/// "lat 12.3 lon -45.6", "12.3, -45.6" and the like → (lat, lon): the
/// first two numbers, the first the latitude.
pub fn parse_lat_lon(s: &str) -> Option<LatLon> {
    let mut nums = s
        .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .filter_map(|t| t.parse::<f64>().ok());
    let (lat, lon) = (nums.next()?, nums.next()?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
        .then_some(LatLon { lat, lon })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_round_trips() {
        let site = LatLon {
            lat: 51.0,
            lon: -1.0,
        };
        for (x, y) in [(0.0, 0.0), (10.0, 0.0), (-120.0, 85.0), (200.0, -150.0)] {
            let p = from_nm(site, x, y);
            let (bx, by) = to_nm(site, p);
            assert!((bx - x).abs() < 1e-6 && (by - y).abs() < 1e-6, "{x},{y}");
        }
        // One degree of latitude north is 60 NM (near enough, R mean).
        let (x, y) = to_nm(
            site,
            LatLon {
                lat: 52.0,
                lon: -1.0,
            },
        );
        assert!(x.abs() < 1e-6 && (y - 60.04).abs() < 0.05, "{x} {y}");
    }

    #[test]
    fn tiles_round_trip() {
        let p = LatLon {
            lat: 51.5,
            lon: -0.12,
        };
        let (tx, ty) = tile_of(p, 10);
        let q = point_of(tx, ty, 10);
        assert!((q.lat - p.lat).abs() < 1e-9 && (q.lon - p.lon).abs() < 1e-9);
        // The tile it is in, at zoom 10 (the slippy-map formula worked
        // separately: 511.66, 340.54).
        assert_eq!((tx as u32, ty as u32), (511, 340));
    }

    #[test]
    fn dark_turns_lightness_over_keeping_hue() {
        // OSM's land, sea and white roads, and black lettering.
        assert_eq!(darken([242, 239, 233, 255]), [22, 19, 13, 255]);
        assert_eq!(darken([170, 211, 223, 255]), [32, 73, 85, 255]);
        assert_eq!(darken([255, 255, 255, 255]), [0, 0, 0, 255]);
        assert_eq!(darken([0, 0, 0, 7]), [255, 255, 255, 7]);
    }

    #[test]
    fn reads_lat_lon_as_written() {
        let p = parse_lat_lon("lat -12.345678°  lon 123.456789°").unwrap();
        assert_eq!((p.lat, p.lon), (-12.345678, 123.456789));
        let p = parse_lat_lon("-31.5, 115.25").unwrap();
        assert_eq!((p.lat, p.lon), (-31.5, 115.25));
        assert!(parse_lat_lon("123.4").is_none());
        assert!(parse_lat_lon("200, 10").is_none());
    }
}
