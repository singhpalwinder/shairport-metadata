//! 32x32 artwork processing: dominant colour for the lights (port of
//! utils/imageProcessor.py) and a tone-mapped RGB565 frame for the matrix.

use image::imageops::{self, FilterType};
use image::{ImageReader, RgbImage};
use std::io::Cursor;

pub const SIZE: u32 = 32;

/// Overall matrix level, applied here (with rounding) instead of on the
/// panel so no 565 levels are thrown away. ~70% keeps the PSU headroom the
/// firmware's BRIGHTNESS=180 used to provide.
pub const MATRIX_GAIN: f32 = 0.70;

const LUMA: [f32; 3] = [0.299, 0.587, 0.114];
/// Below this luma, chroma fades out so dark areas render neutral instead of
/// as single-channel (usually red) LEDs.
const CHROMA_FLOOR: f32 = 48.0;
/// Defaults for the env-tunable look (MATRIX_GAMMA / MATRIX_SATURATION).
/// LEDs are linear in light, artwork is gamma-encoded: without decoding,
/// mid-tones come out too bright and colours look washed out.
const DEFAULT_GAMMA: f32 = 2.0;
const DEFAULT_SATURATION: f32 = 1.35;

fn env_f32(key: &str, default: f32) -> f32 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}
/// Cap on the auto-levels stretch so flat/noisy covers aren't blown up.
const MAX_LEVELS_GAIN: f32 = 4.0;

pub struct ImageProcessor {
    img: RgbImage,             // Lanczos thumbnail, used for the light colour
    area: Vec<[f32; 3]>,       // box-filtered 32x32, used for the matrix
}

impl ImageProcessor {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let decoded = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())?
            .decode()
            .map_err(|e| e.to_string())?
            .to_rgb8();
        Ok(Self::from_rgb(decoded))
    }

    pub fn from_rgb(src: RgbImage) -> Self {
        let img = imageops::resize(&src, SIZE, SIZE, FilterType::Lanczos3);
        let area = box_downscale(&src, SIZE as usize);
        Self { img, area }
    }

    // ---------- lights ----------

    /// Most "colourful" of the k-means clusters, skipping near-black/white and
    /// greys; falls back to green like the Python version.
    pub fn dominant_color(&self, k: usize) -> (u8, u8, u8) {
        let resized = imageops::resize(&self.img, 50, 50, FilterType::Triangle);
        let pixels: Vec<[f64; 3]> = resized
            .pixels()
            .map(|p| [p[0] as f64, p[1] as f64, p[2] as f64])
            .collect();

        let (centers, counts) = kmeans(&pixels, k, 300);
        let mut order: Vec<usize> = (0..centers.len()).collect();
        order.sort_by(|&a, &b| counts[b].cmp(&counts[a]));

        // keyed by truncated variance; later (less frequent) clusters overwrite
        // equal keys, same as the Python dict
        let mut valid: Vec<(i64, (u8, u8, u8))> = Vec::new();
        for &i in &order {
            let c = centers[i];
            let rgb = (c[0] as u8, c[1] as u8, c[2] as u8);
            let Some(variance) = color_variance(rgb) else { continue };
            if !is_blk_white(rgb) && variance > 5.0 {
                let key = variance as i64;
                valid.retain(|(k, _)| *k != key);
                valid.push((key, rgb));
            }
        }

        match valid.iter().max_by_key(|(k, _)| *k) {
            Some(&(v, rgb)) => {
                println!("returning chosen color {rgb:?} with variance {v}");
                rgb
            }
            None => {
                println!("Could not determine dominant color defaulting to green");
                (3, 137, 2)
            }
        }
    }

    // ---------- matrix ----------

    /// Big-endian RGB565 frame (row-major, 2048 bytes) for the matrix portal.
    pub fn matrix_frame(&self) -> Vec<u8> {
        let gamma = env_f32("MATRIX_GAMMA", DEFAULT_GAMMA);
        let saturation = env_f32("MATRIX_SATURATION", DEFAULT_SATURATION);
        let mut px = self.area.clone();
        auto_levels(&mut px);
        let mut out = Vec::with_capacity(px.len() * 2);
        for p in px.iter_mut() {
            let l = luma(*p);
            let chroma = saturation * (l / CHROMA_FLOOR).clamp(0.0, 1.0);
            let mut c = p.map(|v| l + chroma * (v - l));
            clip_preserving_hue(&mut c, l);
            let [r, g, b] = c.map(|v| 255.0 * (v / 255.0).powf(gamma) * MATRIX_GAIN);
            out.extend_from_slice(&pack565(r, g, b).to_be_bytes());
        }
        out
    }
}

fn luma(p: [f32; 3]) -> f32 {
    p[0] * LUMA[0] + p[1] * LUMA[1] + p[2] * LUMA[2]
}

/// Area average: every output pixel is the mean of the source pixels it
/// covers, which is what one LED should show.
fn box_downscale(src: &RgbImage, n: usize) -> Vec<[f32; 3]> {
    let (w, h) = (src.width() as usize, src.height() as usize);
    let mut out = vec![[0.0f32; 3]; n * n];
    for oy in 0..n {
        let (y0, y1) = (oy * h / n, ((oy + 1) * h / n).max(oy * h / n + 1).min(h));
        for ox in 0..n {
            let (x0, x1) = (ox * w / n, ((ox + 1) * w / n).max(ox * w / n + 1).min(w));
            let mut acc = [0.0f64; 3];
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = src.get_pixel(x as u32, y as u32);
                    for c in 0..3 {
                        acc[c] += p[c] as f64;
                    }
                }
            }
            let cnt = ((y1 - y0) * (x1 - x0)) as f64;
            out[oy * n + ox] = acc.map(|a| (a / cnt) as f32);
        }
    }
    out
}

fn percentile(sorted: &[f32], q: f32) -> f32 {
    let i = (q * (sorted.len() - 1) as f32).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

/// Stretch luma so the 1st..99.5th percentile spans the full range, with the
/// same offset/gain on every channel so hues are kept.
fn auto_levels(px: &mut [[f32; 3]]) {
    let mut l: Vec<f32> = px.iter().map(|p| luma(*p)).collect();
    l.sort_by(f32::total_cmp);
    let lo = percentile(&l, 0.01);
    let hi = percentile(&l, 0.995);
    let gain = (255.0 / (hi - lo).max(1.0)).min(MAX_LEVELS_GAIN);
    for p in px.iter_mut() {
        *p = p.map(|v| (v - lo) * gain);
    }
}

/// Bring out-of-range pixels back by scaling chroma toward luma rather than
/// clamping each channel, so highlights keep their hue.
fn clip_preserving_hue(c: &mut [f32; 3], l: f32) {
    let l = l.clamp(0.0, 255.0);
    let mut t: f32 = 1.0;
    for &v in c.iter() {
        if v > 255.0 {
            t = t.min((255.0 - l) / (v - l));
        } else if v < 0.0 {
            t = t.min(l / (l - v));
        }
    }
    for v in c.iter_mut() {
        *v = (l + t * (*v - l)).clamp(0.0, 255.0);
    }
}

/// Round (not truncate) to 5/6/5 bits.
fn pack565(r: f32, g: f32, b: f32) -> u16 {
    let q = |v: f32, max: f32| (v * max / 255.0).round().clamp(0.0, max) as u16;
    (q(r, 31.0) << 11) | (q(g, 63.0) << 5) | q(b, 31.0)
}

fn is_blk_white((r, g, b): (u8, u8, u8)) -> bool {
    let brightness = (r as f64 + g as f64 + b as f64) / 3.0;
    let spread = r.max(g).max(b) - r.min(g).min(b);
    brightness < 15.0 || (brightness > 220.0 && spread < 20)
}

/// Coefficient of variation (%) across the three channels.
fn color_variance((r, g, b): (u8, u8, u8)) -> Option<f64> {
    let v = [r as f64, g as f64, b as f64];
    let mean = v.iter().sum::<f64>() / 3.0;
    if mean == 0.0 {
        return None;
    }
    let std = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / 3.0).sqrt();
    Some(std / mean * 100.0)
}

/// Lloyd's k-means with deterministic k-means++ seeding.
fn kmeans(points: &[[f64; 3]], k: usize, max_iter: usize) -> (Vec<[f64; 3]>, Vec<usize>) {
    let dist2 = |a: &[f64; 3], b: &[f64; 3]| {
        (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)
    };
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng >> 11) as f64 / (1u64 << 53) as f64
    };

    let mut centers = vec![points[(next() * points.len() as f64) as usize]];
    let mut d: Vec<f64> = points.iter().map(|p| dist2(p, &centers[0])).collect();
    while centers.len() < k {
        let total: f64 = d.iter().sum();
        if total == 0.0 {
            break; // fewer distinct colours than k
        }
        let mut target = next() * total;
        let mut idx = points.len() - 1;
        for (i, &di) in d.iter().enumerate() {
            target -= di;
            if target <= 0.0 {
                idx = i;
                break;
            }
        }
        centers.push(points[idx]);
        for (di, p) in d.iter_mut().zip(points) {
            *di = di.min(dist2(p, &centers[centers.len() - 1]));
        }
    }

    let mut labels = vec![0usize; points.len()];
    for _ in 0..max_iter {
        let mut changed = false;
        for (l, p) in labels.iter_mut().zip(points) {
            let best = (0..centers.len())
                .min_by(|&a, &b| dist2(p, &centers[a]).total_cmp(&dist2(p, &centers[b])))
                .unwrap();
            if *l != best {
                *l = best;
                changed = true;
            }
        }
        let mut sums = vec![[0.0f64; 3]; centers.len()];
        let mut n = vec![0usize; centers.len()];
        for (&l, p) in labels.iter().zip(points) {
            for c in 0..3 {
                sums[l][c] += p[c];
            }
            n[l] += 1;
        }
        for i in 0..centers.len() {
            if n[i] > 0 {
                centers[i] = sums[i].map(|s| s / n[i] as f64);
            }
        }
        if !changed {
            break;
        }
    }

    let mut counts = vec![0usize; centers.len()];
    for &l in &labels {
        counts[l] += 1;
    }
    (centers, counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn halves(a: [u8; 3], b: [u8; 3]) -> RgbImage {
        RgbImage::from_fn(SIZE, SIZE, |x, _| image::Rgb(if x < 24 { a } else { b }))
    }

    fn unpack(frame: &[u8], i: usize) -> (u16, u16, u16) {
        let v = u16::from_be_bytes([frame[i * 2], frame[i * 2 + 1]]);
        (v >> 11, (v >> 5) & 63, v & 31)
    }

    #[test]
    fn picks_colourful_over_black() {
        let ip = ImageProcessor::from_rgb(halves([5, 5, 5], [200, 30, 40]));
        let (r, g, b) = ip.dominant_color(4);
        assert!(r > 150 && g < 80 && b < 80, "got {r},{g},{b}");
    }

    #[test]
    fn all_grey_falls_back_to_green() {
        let ip = ImageProcessor::from_rgb(halves([128, 128, 128], [250, 250, 250]));
        assert_eq!(ip.dominant_color(4), (3, 137, 2));
    }

    #[test]
    fn matrix_frame_is_2048_bytes() {
        let ip = ImageProcessor::from_rgb(halves([10, 200, 90], [240, 20, 20]));
        assert_eq!(ip.matrix_frame().len(), 2048);
    }

    #[test]
    fn rgb565_rounds() {
        assert_eq!(pack565(255.0, 0.0, 0.0), 0xF800);
        assert_eq!(pack565(4.2, 0.0, 0.0), 1 << 11); // truncation would give 0
    }

    #[test]
    fn dark_cover_is_stretched() {
        // dim warm gradient, like a dark album cover
        let img = RgbImage::from_fn(64, 64, |x, _| {
            let v = (x * 85 / 63) as u8;
            image::Rgb([v + 6, v + 3, v])
        });
        let frame = ImageProcessor::from_rgb(img).matrix_frame();
        let max_r = (0..1024).map(|i| unpack(&frame, i).0).max().unwrap();
        assert!(max_r >= 20, "brightest red level only {max_r}/31");
    }

    #[test]
    fn near_black_is_neutral() {
        // very dark brown next to white so levels don't lift it
        let frame = ImageProcessor::from_rgb(halves([22, 12, 8], [255, 255, 255])).matrix_frame();
        let (r, g, b) = unpack(&frame, 0);
        // compare in 5-bit units (green is 6-bit)
        assert!(r.abs_diff(g / 2) <= 1 && r.abs_diff(b) <= 1, "got {r},{g},{b}");
    }

    #[test]
    fn highlights_keep_hue() {
        let mut c = [300.0, 200.0, 100.0];
        let l = luma(c);
        clip_preserving_hue(&mut c, l);
        assert!(c[0] <= 255.0 && c[0] > c[1] && c[1] > c[2]);
    }

    #[test]
    fn decodes_png_bytes() {
        let mut png = Vec::new();
        halves([1, 2, 3], [4, 5, 6])
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let ip = ImageProcessor::from_bytes(&png).unwrap();
        assert_eq!(ip.img.dimensions(), (SIZE, SIZE));
    }
}
