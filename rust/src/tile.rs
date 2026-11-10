//! Draws the weather tiles that the board loops on its idle clock: an animated icon
//! with the temperature (top right, 32x13) and today's high/low (bottom row, 64x9).
//! Animations are periodic in their frame count, so the board can loop them seamlessly.

use crate::font5x7::GLCD;
use crate::weather::Weather;
use std::f32::consts::TAU;

pub type Rgb = [u8; 3];

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    px: Vec<Rgb>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Self {
        Canvas { w, h, px: vec![[0; 3]; w * h] }
    }

    pub fn set(&mut self, x: i32, y: i32, c: Rgb) {
        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
            self.px[y as usize * self.w + x as usize] = c;
        }
    }

    pub fn get(&self, x: usize, y: usize) -> Rgb {
        self.px[y * self.w + x]
    }

    pub fn text(&mut self, x: i32, y: i32, s: &str, c: Rgb) {
        for (i, ch) in s.chars().enumerate() {
            let code = ch as usize;
            let Some(glyph) = (32..127).contains(&code).then(|| GLCD[code - 32]) else { continue };
            for (col, bits) in glyph.iter().enumerate() {
                for row in 0..8 {
                    if bits & (1 << row) != 0 {
                        self.set(x + i as i32 * 6 + col as i32, y + row, c);
                    }
                }
            }
        }
    }

    /// Big-endian RGB565, row by row, as the board expects.
    pub fn rgb565_be(&self) -> Vec<u8> {
        self.px
            .iter()
            .flat_map(|&[r, g, b]| {
                let v = ((r as u16 & 0xF8) << 8) | ((g as u16 & 0xFC) << 3) | (b as u16 >> 3);
                v.to_be_bytes()
            })
            .collect()
    }
}

pub fn text_width(s: &str) -> i32 {
    s.chars().count() as i32 * 6 - 1
}

fn scale(c: Rgb, k: f32) -> Rgb {
    c.map(|v| (v as f32 * k.clamp(0.0, 1.0)).round() as u8)
}

fn disc(c: &mut Canvas, cx: f32, cy: f32, r: f32, col: Rgb) {
    for y in 0..c.h as i32 {
        for x in 0..c.w as i32 {
            if ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt() <= r {
                c.set(x, y, col);
            }
        }
    }
}

const CLOUD: [&str; 6] = ["....###....", "..#######..", ".#########.", "###########", "###########", ".#########."];

fn cloud(c: &mut Canvas, ox: i32, oy: i32, col: Rgb) {
    for (y, row) in CLOUD.iter().enumerate() {
        for (x, ch) in row.chars().enumerate() {
            if ch == '#' {
                c.set(ox + x as i32, oy + y as i32, col);
            }
        }
    }
}

fn rays(c: &mut Canvas, cx: f32, cy: f32, r0: f32, r1: f32, turn: f32, col: Rgb) {
    for k in 0..8 {
        let a = (k as f32 / 8.0 + turn) * TAU;
        for r in [r0, r1] {
            c.set((cx + a.cos() * r - 0.5).round() as i32, (cy + a.sin() * r - 0.5).round() as i32, col);
        }
    }
}

fn moon(c: &mut Canvas, cx: f32, cy: f32, r: f32, col: Rgb) {
    for y in 0..c.h as i32 {
        for x in 0..c.w as i32 {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let inside = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() <= r;
            let bite = ((fx - cx - r * 0.55).powi(2) + (fy - cy + r * 0.35).powi(2)).sqrt() <= r * 0.85;
            if inside && !bite {
                c.set(x, y, col);
            }
        }
    }
}

/// Falling particles in `cols`, each column offset in phase; `per_loop` drops per loop.
fn fall(c: &mut Canvas, p: f32, cols: &[i32], top: i32, depth: f32, per_loop: f32, tail: bool, col: Rgb) {
    for (i, &x) in cols.iter().enumerate() {
        let t = (p * per_loop + i as f32 * 0.37).fract();
        let y = top + (t * depth) as i32;
        c.set(x, y, col);
        if tail {
            c.set(x, y - 1, scale(col, 0.45));
        }
    }
}

const SUN: Rgb = [255, 196, 50];
const RAY: Rgb = [255, 140, 40];
const MOON: Rgb = [235, 225, 170];
const CLOUD_LIGHT: Rgb = [215, 220, 230];
const CLOUD_GREY: Rgb = [140, 148, 165];
const CLOUD_DARK: Rgb = [105, 110, 128];
const RAIN: Rgb = [80, 160, 255];
const SNOW: Rgb = [235, 240, 255];
const BOLT: Rgb = [255, 225, 70];

/// Frames in one loop of each condition's icon (100 ms each).
pub fn frame_count(condition: u8) -> usize {
    match condition {
        1 | 2 => 32,  // slow sun rotation, star twinkle
        9 | 10 => 30, // one lightning strike per 3 seconds
        _ => 16,
    }
}

/// Draws a condition's icon into a 13x13 canvas for frame `f` of `n`.
pub fn icon(c: &mut Canvas, w: &Weather, f: usize, n: usize) {
    let p = f as f32 / n as f32;
    let sway = (p * TAU).sin();
    match w.condition {
        1 | 2 if w.night || w.condition == 2 => {
            moon(c, 6.5, 6.5, 4.6, MOON);
            for (i, (x, y)) in [(1, 1), (11, 2), (10, 11), (1, 9)].into_iter().enumerate() {
                let k = 0.25 + 0.75 * ((p + i as f32 * 0.27) * TAU).sin().max(0.0);
                c.set(x, y, scale([255, 255, 230], k));
            }
        }
        1 => {
            rays(c, 6.5, 6.5, 4.6, 5.8, p / 8.0, RAY);
            disc(c, 6.5, 6.5, 3.0, SUN);
        }
        3 => {
            if w.night {
                moon(c, 8.5, 4.5, 3.4, MOON);
            } else {
                rays(c, 8.5, 4.5, 3.5, 4.4, p / 8.0, RAY);
                disc(c, 8.5, 4.5, 2.4, SUN);
            }
            cloud(c, (sway * 1.0).round() as i32, 6, CLOUD_LIGHT);
        }
        4 => {
            cloud(c, 2 + sway.round() as i32, 1, CLOUD_GREY);
            cloud(c, -(sway.round() as i32), 6, CLOUD_LIGHT);
        }
        5 => {
            cloud(c, 1, 0, CLOUD_GREY);
            fall(c, p, &[2, 5, 8, 11], 7, 6.0, 1.0, true, RAIN);
        }
        6 => {
            cloud(c, 1, 0, CLOUD_DARK);
            fall(c, p, &[1, 3, 5, 7, 9, 11], 7, 6.0, 2.0, true, RAIN);
        }
        7 | 13 => {
            cloud(c, 1, 0, CLOUD_GREY);
            let ice = if w.condition == 13 { [200, 230, 255] } else { SNOW };
            for (i, x) in [2, 6, 10].into_iter().enumerate() {
                let t = (p + i as f32 * 0.33).fract();
                let wobble = ((p * 2.0 + i as f32 * 0.5) * TAU).sin().round() as i32;
                c.set(x + wobble, 7 + (t * 6.0) as i32, ice);
            }
        }
        8 => {
            cloud(c, 1, 0, CLOUD_GREY);
            fall(c, p, &[3, 9], 7, 6.0, 1.0, true, RAIN);
            fall(c, (p + 0.5).fract(), &[6], 7, 6.0, 1.0, false, SNOW);
        }
        9 | 10 => {
            let flash = matches!(f, 0 | 1 | 3);
            cloud(c, 1, 0, if flash { CLOUD_GREY } else { CLOUD_DARK });
            if w.condition == 10 {
                fall(c, p, &[2, 10], 7, 6.0, 3.0, true, RAIN);
            }
            if flash {
                for (x, y) in [(7, 6), (6, 7), (5, 8), (6, 8), (7, 8), (6, 9), (5, 10), (4, 11)] {
                    c.set(x, y, BOLT);
                }
            }
        }
        11 => {
            for (i, y) in [2, 5, 8, 11].into_iter().enumerate() {
                let s = (1.5 + 1.5 * ((p + i as f32 * 0.25) * TAU).sin()).round() as i32;
                for x in s..s + 9 {
                    c.set(x, y, [170, 175, 185]);
                }
            }
        }
        12 => {
            for (i, (y, len)) in [(3, 8), (7, 10), (11, 6)].into_iter().enumerate() {
                let head = ((p + i as f32 * 0.33).fract() * (13 + len) as f32) as i32;
                for k in 0..len {
                    c.set(head - k, y, scale([170, 220, 240], 1.0 - k as f32 / len as f32));
                }
            }
        }
        _ => c.text(4, 3, "!", RAY),
    }
}

fn round_temp(v: f64) -> String {
    format!("{}", v.round() as i64)
}

/// Top-right tile: icon and current temperature, one entry per frame.
pub fn top_frames(w: &Weather) -> Vec<Canvas> {
    let n = frame_count(w.condition);
    (0..n)
        .map(|f| {
            // Draw the icon in its own 13x13 box so nothing spills into the temperature.
            let mut ic = Canvas::new(13, 13);
            icon(&mut ic, w, f, n);
            let mut c = Canvas::new(32, 13);
            for y in 0..13 {
                for x in 0..13 {
                    c.set(x as i32, y as i32, ic.get(x, y));
                }
            }
            let t = round_temp(w.temp);
            let degree = t.len() <= 2; // no room for the ring on "-12" or "102"
            let width = text_width(&t) + if degree { 4 } else { 0 };
            let x = 14 + (18 - width) / 2;
            c.text(x, 3, &t, [255, 255, 255]);
            if degree {
                let dx = x + text_width(&t) + 1;
                for (ax, ay) in [(1, 0), (0, 1), (2, 1), (1, 2)] {
                    c.set(dx + ax, 3 + ay, [255, 255, 255]);
                }
            }
            c
        })
        .collect()
}

/// Bottom row: today's high and low, or the condition when there's no forecast.
pub fn bottom_frame(w: &Weather) -> Canvas {
    let mut c = Canvas::new(64, 9);
    match w.high_low {
        Some((hi, lo)) => {
            let (h, l) = (format!("H{}", round_temp(hi)), format!("L{}", round_temp(lo)));
            let x = (64 - text_width(&format!("{h} {l}"))) / 2;
            c.text(x, 1, &h, [255, 170, 120]);
            c.text(x + (h.len() as i32 + 1) * 6, 1, &l, [130, 190, 255]);
        }
        None => {
            let word = condition_word(w.condition);
            c.text((64 - text_width(word)) / 2, 1, word, [200, 205, 215]);
        }
    }
    c
}

fn condition_word(condition: u8) -> &'static str {
    match condition {
        1 => "SUNNY",
        2 => "CLEAR",
        3 => "PARTLY CLOUDY",
        4 => "CLOUDY",
        5 => "RAIN",
        6 => "HEAVY RAIN",
        7 => "SNOW",
        8 => "SLEET",
        9 | 10 => "STORMS",
        11 => "FOG",
        12 => "WINDY",
        13 => "HAIL",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wx(condition: u8) -> Weather {
        Weather { condition, night: false, temp: 59.6, high_low: Some((63.0, 48.0)), celsius: false }
    }

    #[test]
    fn every_condition_draws_something_and_loops() {
        for cond in 0..=14 {
            let frames = top_frames(&wx(cond));
            assert_eq!(frames.len(), frame_count(cond));
            let lit = |c: &Canvas| (0..13).flat_map(|y| (0..13).map(move |x| (x, y))).filter(|&(x, y)| c.get(x, y) != [0; 3]).count();
            assert!(lit(&frames[0]) > 0, "condition {cond} icon is empty");
        }
    }

    #[test]
    fn tiles_have_the_board_sizes() {
        let top = top_frames(&wx(1));
        assert_eq!(top[0].rgb565_be().len(), 32 * 13 * 2);
        assert_eq!(bottom_frame(&wx(1)).rgb565_be().len(), 64 * 9 * 2);
    }

    #[test]
    fn white_is_ffff() {
        let mut c = Canvas::new(1, 1);
        c.set(0, 0, [255, 255, 255]);
        assert_eq!(c.rgb565_be(), vec![0xFF, 0xFF]);
    }
}
