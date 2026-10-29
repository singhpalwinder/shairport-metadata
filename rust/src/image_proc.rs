//! Port of utils/imageProcessor.py: 32x32 thumbnail, dominant colour for the
//! lights, and an enhanced + dithered RGB565 frame for the matrix.

use image::imageops::{self, FilterType};
use image::{ImageReader, RgbImage};
use std::io::Cursor;

pub const SIZE: u32 = 32;

pub struct ImageProcessor {
    img: RgbImage,
}

impl ImageProcessor {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let decoded = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())?
            .decode()
            .map_err(|e| e.to_string())?;
        let img = imageops::resize(&decoded.to_rgb8(), SIZE, SIZE, FilterType::Lanczos3);
        Ok(Self { img })
    }

    #[cfg(test)]
    pub fn from_rgb(img: RgbImage) -> Self {
        Self { img }
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

    /// Saturation x1.5, brightness x0.4, contrast x1.5 (PIL ImageEnhance
    /// semantics), then Floyd-Steinberg down to 5 bits per channel.
    pub fn enhance_image(&self) -> RgbImage {
        let mut img = self.img.clone();

        let grey: Vec<u8> = img.pixels().map(|p| luma(p.0)).collect();
        for (p, &g) in img.pixels_mut().zip(&grey) {
            for c in p.0.iter_mut() {
                *c = blend(g, *c, 1.5);
            }
        }

        for p in img.pixels_mut() {
            for c in p.0.iter_mut() {
                *c = blend(0, *c, 0.4);
            }
        }

        let sum: u64 = img.pixels().map(|p| luma(p.0) as u64).sum();
        let mean = (sum as f64 / (SIZE * SIZE) as f64 + 0.5) as u8;
        for p in img.pixels_mut() {
            for c in p.0.iter_mut() {
                *c = blend(mean, *c, 1.5);
            }
        }

        floyd_steinberg(&img, 5)
    }
}

/// PIL "L" conversion (ITU-R 601-2, fixed point).
fn luma([r, g, b]: [u8; 3]) -> u8 {
    ((r as u32 * 19595 + g as u32 * 38470 + b as u32 * 7471 + 0x8000) >> 16) as u8
}

/// PIL Image.blend(degenerate, image, alpha) for one channel.
fn blend(degenerate: u8, v: u8, alpha: f32) -> u8 {
    let t = degenerate as f32 + alpha * (v as f32 - degenerate as f32);
    t.clamp(0.0, 255.0) as u8
}

fn floyd_steinberg(img: &RgbImage, bits: u32) -> RgbImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let max = ((1u32 << bits) - 1) as f32;
    let mut buf: Vec<[f32; 3]> = img
        .pixels()
        .map(|p| [p[0] as f32, p[1] as f32, p[2] as f32])
        .collect();

    let quantize = |v: f32| (v * max / 255.0).round_ties_even() * (255.0 / max);

    for y in 0..h {
        for x in 0..w {
            let old = buf[y * w + x];
            let new = old.map(quantize);
            buf[y * w + x] = new;
            let err = [old[0] - new[0], old[1] - new[1], old[2] - new[2]];

            let mut spread = |xx: usize, yy: usize, f: f32| {
                let p = &mut buf[yy * w + xx];
                for c in 0..3 {
                    p[c] += err[c] * f;
                }
            };
            if x + 1 < w {
                spread(x + 1, y, 7.0 / 16.0);
            }
            if y + 1 < h {
                if x > 0 {
                    spread(x - 1, y + 1, 3.0 / 16.0);
                }
                spread(x, y + 1, 5.0 / 16.0);
                if x + 1 < w {
                    spread(x + 1, y + 1, 1.0 / 16.0);
                }
            }
        }
    }

    RgbImage::from_fn(w as u32, h as u32, |x, y| {
        let p = buf[y as usize * w + x as usize];
        image::Rgb(p.map(|v| v.clamp(0.0, 255.0) as u8))
    })
}

/// Big-endian RGB565, row-major — what the matrix portal expects.
pub fn to_rgb565_be(img: &RgbImage) -> Vec<u8> {
    let mut out = Vec::with_capacity((img.width() * img.height() * 2) as usize);
    for p in img.pixels() {
        let v = ((p[0] as u16 & 0xF8) << 8) | ((p[1] as u16 & 0xFC) << 3) | (p[2] as u16 >> 3);
        out.extend_from_slice(&v.to_be_bytes());
    }
    out
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
        assert_eq!(to_rgb565_be(&ip.enhance_image()).len(), 2048);
    }

    #[test]
    fn rgb565_packing() {
        let img = RgbImage::from_pixel(1, 1, image::Rgb([255, 0, 0]));
        assert_eq!(to_rgb565_be(&img), vec![0xF8, 0x00]);
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
