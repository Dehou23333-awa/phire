//! 官方 mask / EdgeMask / GlowMask 三趟的 CPU 等价实现。
//!
//! 用位行（`u64`）存二值遮罩，让 3×3 膨胀在没有任何离屏 FBO 的情况下也够便宜。
//! 算法逐行对应 Phira Pro 的 `block_mask.rs`（其正确性已由它的 GPU oracle 对照过
//! 官方 GLSL），这里只改了模块路径与注释。
//!
//! 产出两张贴图给 `block_shader.rs`：
//!
//! * `rgba`     —— R=启用后的 Compose、G=Edge、B=Glow、A=未启用 Compose
//! * `aux_rgba` —— R=Ready-only 法线、G=Ready-only subtract、B=未启用 Compose、A=hover

use super::Zone;
use once_cell::sync::Lazy;

/// 官方位移贴图，**上下翻转后**才是 GPU 采样看到的行序
/// （导出的 PNG 是自上而下，GLES 纹理坐标自下而上）。
static DISPLACE: Lazy<image::RgbImage> = Lazy::new(|| {
    let source = image::load_from_memory(include_bytes!("../../../assets/blockarea/BlockNoise1.png"))
        .expect("官方 BlockNoise1.png 缺失，见 docs/block-area/extract_official_assets.py")
        .to_rgb8();
    image::imageops::flip_vertical(&source)
});

#[derive(Default)]
pub(super) struct Masks {
    /// 效果遮罩（RGBA），尺寸是 1/8 分辨率再 2 倍 = 1/4。
    pub rgba: Vec<u8>,
    /// 辅助遮罩：Ready-only 法线 R、Ready-only 后处理 subtract G、未启用 Compose、hover。
    pub aux_rgba: Vec<u8>,
    /// 点采样出来的六个相机源，按 启用法线/启用subtract/未启用法线/未启用subtract 打包。
    pub sources_rgba: Vec<u8>,
    /// 未启用 subtract 的原始 G 通道（后处理前）。
    pub raw_disabled_green: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub revision: u64,
    active: Vec<u64>,
    ping: Vec<u64>,
    pong: Vec<u64>,
    last_dim: (usize, usize, f32),
    last_zones: Vec<Zone>,
    last_time: Option<f32>,
    layers: [Vec<u8>; 6],
    ready_green: Vec<u8>,
    gray_ping: Vec<u8>,
    gray_pong: Vec<u8>,
    warp_x: Vec<(f32, usize, usize)>,
    warp_y: Vec<(f32, usize)>,
    enabled_compose: Vec<u8>,
    point_aux: Vec<[u8; 3]>,
}

impl Masks {
    /// 在 1/8 分辨率上栅格化六个相机源，再做 Compose 位移与 Edge/Glow，最后放大 2 倍。
    pub fn render_displaced(&mut self, width: usize, height: usize, aspect: f32, zones: &[Zone], time: f32) {
        let (bw, bh) = ((width / 8).max(1), (height / 8).max(1));
        let (ew, eh) = (bw * 2, bh * 2);
        // 同一维度 / 同一组区域 / 同一时刻可以直接复用上一帧。
        if self.last_dim == (ew, eh, aspect) && self.last_zones == zones && self.last_time == Some(time) {
            return;
        }
        if self.last_dim != (ew, eh, aspect) || self.last_zones != zones {
            self.last_dim = (ew, eh, aspect);
            self.last_zones.clear();
            self.last_zones.extend_from_slice(zones);
            for layer in &mut self.layers {
                layer.resize(bw * bh, 0);
                layer.fill(0);
            }
            self.raw_disabled_green.resize(bw * bh, 0);
            self.raw_disabled_green.fill(0);
            self.ready_green.resize(bw * bh, 0);
            self.ready_green.fill(0);
            // 官方六个相机分别抓：启用 法线/subtract、未启用 法线/subtract（含 Ready）、
            // Ready-only 法线/subtract。BlockSprite 用 SrcAlpha/One 混合，
            // 每个 subtract sprite 的 alpha 固定 0.1。
            for z in zones {
                let layer = if z.active { usize::from(z.invert) } else { 2 + usize::from(z.invert) };
                let opacity = if z.invert { 0.1 } else { z.opacity };
                raster_rows(bw, bh, aspect, z, |y, first, last| {
                    for x in first..last {
                        let i = y * bw + x;
                        self.layers[layer][i] = unorm(self.layers[layer][i] as f32 / 255. + opacity);
                        if z.invert && !z.active {
                            self.raw_disabled_green[i] = unorm(self.raw_disabled_green[i] as f32 / 255. + 0.1 * z.opacity);
                        }
                        if z.ready {
                            let ready = 4 + usize::from(z.invert);
                            self.layers[ready][i] = unorm(self.layers[ready][i] as f32 / 255. + opacity);
                            if z.invert {
                                self.ready_green[i] = unorm(self.ready_green[i] as f32 / 255. + 0.1 * z.opacity);
                            }
                        }
                    }
                });
            }
            self.sources_rgba.resize(bw * bh * 4, 0);
            self.enabled_compose.resize(bw * bh, 0);
            self.point_aux.resize(bw * bh, [0; 3]);
            for i in 0..bw * bh {
                for c in 0..4 {
                    self.sources_rgba[i * 4 + c] = self.layers[c][i];
                }
                self.enabled_compose[i] = self.layers[0][i].abs_diff(if subtract_enabled(self.layers[1][i]) == 1. { 255 } else { 0 });
                let disabled = if self.layers[2][i] == 0 && self.layers[3][i] == 0 && self.raw_disabled_green[i] == 0 {
                    0
                } else {
                    let (sr, sg) = subtract_disabled(self.layers[3][i], self.raw_disabled_green[i]);
                    unorm((sr * sg - self.layers[2][i] as f32 / 255.).abs())
                };
                let ready_s = if self.ready_green[i] == 0 {
                    0
                } else {
                    unorm(subtract_disabled(self.layers[5][i], self.ready_green[i]).1)
                };
                self.point_aux[i] = [self.layers[4][i], ready_s, disabled];
            }
        }
        self.width = ew;
        self.height = eh;
        self.rgba.resize(ew * eh * 4, 0);
        self.rgba.fill(0);
        self.aux_rgba.resize(ew * eh * 4, 0);
        self.aux_rgba.fill(0);
        let stride = ew.div_ceil(64);
        self.active.resize(stride * eh, 0);
        self.active.fill(0);
        // 官方两次位移采样共用同一个 Y 坐标，所以按行/列预计算镜像纹理下标，
        // 免得每个遮罩像素都做四次浮点取余 + 查表。
        let texture = &*DISPLACE;
        let pixels = texture.as_raw();
        let texture_stride = texture.width() as usize * 3;
        let d = 0.70703125_f32;
        let dt = d * (time / 20. * 2.59);
        self.warp_x.clear();
        self.warp_y.clear();
        self.warp_x.extend((0..bw).map(|x| {
            let u = (x as f32 + 0.5) / bw as f32;
            (u, noise_index(dt + u * 2.13, texture.width()) as usize * 3, noise_index(-dt + u * 2.13, texture.width()) as usize * 3)
        }));
        self.warp_y.extend((0..bh).map(|y| {
            let v = (y as f32 + 0.5) / bh as f32;
            (v, noise_index(dt + v * 1.02, texture.height()) as usize * texture_stride)
        }));
        static CENTERED_NOISE: Lazy<[f32; 256]> = Lazy::new(|| std::array::from_fn(|value| medium(medium(value as f32 / 255.) - 0.5)));
        let centered = &*CENTERED_NOISE;
        for y in 0..bh {
            for x in 0..bw {
                let (u, xa, xb) = self.warp_x[x];
                let (v, row) = self.warp_y[y];
                let a = centered[pixels[row + xa] as usize];
                let b = centered[pixels[row + xb] as usize];
                let duv = [(d * a + b * -d) * 0.1 + u, (d * a + b * d) * 0.1 + v];
                let sx = (duv[0] * bw as f32).floor().clamp(0., (bw - 1) as f32) as usize;
                let sy = (duv[1] * bh as f32).floor().clamp(0., (bh - 1) as f32) as usize;
                let di = sy * bw + sx;
                let mask = self.enabled_compose[di];
                let [ready_n, ready_s, disabled] = self.point_aux[y * bw + x];
                let rgba = [mask, 0, 0, disabled, mask, 0, 0, disabled];
                let aux = [ready_n, ready_s, disabled, 0, ready_n, ready_s, disabled, 0];
                for yy in [y * 2, y * 2 + 1] {
                    let dst = (yy * ew + x * 2) * 4;
                    self.rgba[dst..dst + 8].copy_from_slice(&rgba);
                    self.aux_rgba[dst..dst + 8].copy_from_slice(&aux);
                    if mask != 0 {
                        self.active[yy * stride + x * 2 / 64] |= 3 << ((x * 2) % 64);
                    }
                }
            }
        }
        self.render_rings();
        self.last_time = Some(time);
        self.revision = self.revision.wrapping_add(1);
    }

    /// EdgeMask + GlowMask：对二值遮罩做 6 趟 3×3 膨胀，逐趟取「新鼓出来的一圈」。
    fn render_rings(&mut self) {
        let (width, height) = (self.width, self.height);
        // 部分透明（subtract 的 0.1 权重）走灰度慢路径。
        if self.rgba.chunks_exact(4).any(|p| p[0] != 0 && p[0] != 255) {
            self.render_gray_rings();
            return;
        }
        let stride = width.div_ceil(64);
        self.ping.clone_from(&self.active);
        self.pong.resize(self.active.len(), 0);
        for (pass, weight) in glow_weights().into_iter().enumerate() {
            if weight < 0.01 {
                break;
            }
            dilate(&self.ping, &mut self.pong, width, height);
            for y in 0..height {
                for word in 0..stride {
                    let i = y * stride + word;
                    let mut ring = self.pong[i] & !self.ping[i];
                    while ring != 0 {
                        let bit = ring.trailing_zeros() as usize;
                        let x = word * 64 + bit;
                        if x < width {
                            let dst = (y * width + x) * 4;
                            if pass == 0 {
                                self.rgba[dst + 1] = 255;
                            }
                            self.rgba[dst + 2] = unorm(weight);
                        }
                        ring &= ring - 1;
                    }
                }
            }
            std::mem::swap(&mut self.ping, &mut self.pong);
        }
    }

    fn render_gray_rings(&mut self) {
        let (w, h) = (self.width, self.height);
        self.gray_ping.resize(w * h, 0);
        self.gray_pong.resize(w * h, 0);
        for (i, p) in self.rgba.chunks_exact(4).enumerate() {
            self.gray_ping[i] = p[0];
        }
        for (pass, weight) in glow_weights().into_iter().enumerate() {
            if weight < 0.01 {
                break;
            }
            for y in 0..h {
                for x in 0..w {
                    let i = y * w + x;
                    let mut maximum = self.gray_ping[i];
                    for yy in y.saturating_sub(1)..=(y + 1).min(h - 1) {
                        for xx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                            maximum = maximum.max(self.gray_ping[yy * w + xx]);
                        }
                    }
                    self.gray_pong[i] = maximum;
                    let delta = (maximum - self.gray_ping[i]) as f32 / 255.;
                    if pass == 0 {
                        self.rgba[i * 4 + 1] = maximum - self.gray_ping[i];
                    }
                    let outside = 1. - self.rgba[i * 4] as f32 / 255.;
                    self.rgba[i * 4 + 2] = unorm(weight * (outside * delta) + self.rgba[i * 4 + 2] as f32 / 255.);
                }
            }
            std::mem::swap(&mut self.gray_ping, &mut self.gray_pong);
        }
    }
}

/// GlowMask 的 6 趟权重：`(6-pass)^2.65 / Σ`。
fn glow_weights() -> [f32; 6] {
    static WEIGHTS: Lazy<[f32; 6]> = Lazy::new(|| {
        let sum: f32 = (1..=6).map(|k| (k as f32).powf(2.65)).sum();
        std::array::from_fn(|pass| ((6 - pass) as f32).powf(2.65) / sum)
    });
    *WEIGHTS
}

#[inline]
fn unorm(v: f32) -> u8 {
    (v.clamp(0., 1.) * 255.).round() as u8
}

/// 官方 `SubtractBlockBlender` 第 0 趟：**阈值带通**，不是奇偶。
///
/// 注意这与 `block.rs` 里输入判定用的奇偶规则**故意不同**。
#[inline]
fn subtract_enabled(red: u8) -> f32 {
    let r = red as f32 / 255.;
    if (0.09..0.12).contains(&r) {
        1.
    } else {
        0.
    }
}

/// 官方 `SubtractBlockBlender` 第 1 趟（未启用版）。
fn subtract_disabled(red: u8, green: u8) -> (f32, f32) {
    let g = green as f32 / 255.;
    let t = ((g - 0.2) * -10.).clamp(0., 1.);
    let r = subtract_enabled(red) + t * t * (3. - 2. * t);
    // 中间结果是 RG16，shader 输出后要 clamp 再 round。
    (unorm(r) as f32 / 255., unorm(g * r * 10.) as f32 / 255.)
}

/// 官方 `BlockCompose` 第 0 趟的位移。
///
/// 原生 `u_xlat16_*` 是 mediump（binary16）而不是 CPU 全精度浮点：导出的 GLES shader 里
/// 逆长度、方向、采样到的噪声、减去 0.5 之后的值各自单独舍入到 f16。
/// 之后的 warp 坐标保持 highp —— 点采样时这一步会影响取到哪个 texel。
#[allow(dead_code)] // 供将来的 GPU oracle 对照用
pub(super) fn compose_uv(uv: [f32; 2], time: f32) -> [f32; 2] {
    let d = medium(0.5 * medium((0.5_f32).sqrt().recip()));
    let t = time / 20. * 2.59;
    let st = [uv[0] * 2.13, uv[1] * 1.02];
    let a = medium(medium(noise([d * t + st[0], d * t + st[1]])) - 0.5);
    let b = medium(medium(noise([-d * t + st[0], -d * t + st[1]])) - 0.5);
    [(d * a + b * -d) * 0.1 + uv[0], (d * a + b * d) * 0.1 + uv[1]]
}

/// 把 f32 按 binary16 精度舍入（10 位尾数、就近取偶）。
fn medium(value: f32) -> f32 {
    let bits = value.to_bits();
    f32::from_bits((bits + 0xfff + ((bits >> 13) & 1)) & !0x1fff)
}

/// 官方纹理是 `Mirror` wrap，这里手工做镜像取模。
fn noise_index(v: f32, size: u32) -> u32 {
    let v = v.rem_euclid(2.);
    let v = if v > 1. { 2. - v } else { v };
    (v * size as f32).floor().clamp(0., (size - 1) as f32) as u32
}

fn noise(uv: [f32; 2]) -> f32 {
    DISPLACE.get_pixel(noise_index(uv[0], DISPLACE.width()), noise_index(uv[1], DISPLACE.height()))[0] as f32 / 255.
}

/// 把一块矩形栅格化成逐行的 `[first, last)` 列区间（旋转矩形用扫描线求交）。
fn raster_rows(width: usize, height: usize, aspect: f32, zone: &Zone, mut write: impl FnMut(usize, usize, usize)) {
    if zone.half.x <= 0. || zone.half.y <= 0. {
        return;
    }
    let (s, c) = zone.angle.sin_cos();
    let y_half = s.abs() * zone.half.x + c.abs() * zone.half.y;
    let start = (((zone.center.y - y_half) * aspect + 1.) * height as f32 * 0.5 - 0.5)
        .ceil()
        .clamp(0., height as f32) as usize;
    let end = ((((zone.center.y + y_half) * aspect + 1.) * height as f32 * 0.5 - 0.5).floor() + 1.).clamp(0., height as f32) as usize;
    for y in start..end {
        let dy = ((y as f32 + 0.5) * 2. / height as f32 - 1.) / aspect - zone.center.y;
        let (mut lo, mut hi) = (-1.0_f32, 1.0_f32);
        for (a, b, half) in [(c, s * dy, zone.half.x), (-s, c * dy, zone.half.y)] {
            if a.abs() < 1e-7 {
                if b.abs() > half {
                    hi = lo - 1.;
                    break;
                }
            } else {
                let a0 = (-half - b) / a + zone.center.x;
                let a1 = (half - b) / a + zone.center.x;
                lo = lo.max(a0.min(a1));
                hi = hi.min(a0.max(a1));
            }
        }
        if lo > hi {
            continue;
        }
        let first = ((lo + 1.) * width as f32 * 0.5 - 0.5).ceil().clamp(0., width as f32) as usize;
        let last = (((hi + 1.) * width as f32 * 0.5 - 0.5).floor() + 1.).clamp(0., width as f32) as usize;
        write(y, first, last);
    }
}

/// 3×3 膨胀，把二值遮罩按位行展开。
fn dilate(source: &[u64], dest: &mut [u64], width: usize, height: usize) {
    let stride = width.div_ceil(64);
    let last_mask = u64::MAX >> ((64 - width % 64) % 64);
    for y in 0..height {
        for x in 0..stride {
            let mut value = 0;
            for row in y.saturating_sub(1)..=(y + 1).min(height - 1) {
                let idx = row * stride + x;
                let mid = source[idx];
                let left = if x > 0 { source[idx - 1] >> 63 } else { 0 };
                let right = if x + 1 < stride { source[idx + 1] << 63 } else { 0 };
                value |= mid | (mid << 1) | left | (mid >> 1) | right;
            }
            dest[y * stride + x] = if x + 1 == stride { value & last_mask } else { value };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Vector;

    /// mediump 中间量的舍入（binary16、就近取偶）。
    #[test]
    fn mediump_intermediates_round_like_binary16() {
        assert_eq!(medium(0.5 * medium((0.5_f32).sqrt().recip())), 0.70703125);
        assert_eq!(medium(medium(155. / 255.) - 0.5), 0.10791015625);
        assert_eq!(medium(1. + 1. / 2048.), 1., "ties round to even");
        assert_eq!(medium(1. + 3. / 2048.), 1.001953125);
    }

    fn zone(x: f32, y: f32, half_x: f32, half_y: f32, angle: f32, invert: bool) -> Zone {
        Zone {
            center: Vector::new(x, y),
            half: Vector::new(half_x, half_y),
            angle,
            invert,
            active: true,
            ready: false,
            opacity: 1.,
        }
    }

    /// 位行膨胀必须与朴素 3×3 参考实现逐像素一致，包括跨 `u64` 边界。
    #[test]
    fn nine_tap_dilation_matches_pixel_reference_across_word_boundaries() {
        for width in [1_usize, 63, 64, 65, 129] {
            let height = 9;
            let stride = width.div_ceil(64);
            let mut source = vec![0_u64; stride * height];
            for y in 0..height {
                for x in 0..width {
                    if (x * 11 + y * 7) % 17 == 0 {
                        source[y * stride + x / 64] |= 1 << (x % 64);
                    }
                }
            }
            let mut dest = vec![0; source.len()];
            dilate(&source, &mut dest, width, height);
            for y in 0..height {
                for x in 0..width {
                    let mut expected = false;
                    for dy in -1..=1 {
                        for dx in -1..=1 {
                            let sx = (x as i32 + dx).clamp(0, width as i32 - 1) as usize;
                            let sy = (y as i32 + dy).clamp(0, height as i32 - 1) as usize;
                            expected |= source[sy * stride + sx / 64] & (1 << (sx % 64)) != 0;
                        }
                    }
                    assert_eq!(dest[y * stride + x / 64] & (1 << (x % 64)) != 0, expected, "{width}: ({x}, {y})");
                }
            }
        }
    }

    /// 六个相机源 = 逐像素的「点在不在矩形里」，subtract 按 0.1 累加后取 unorm。
    #[test]
    fn raw_camera_layers_match_resolved_rectangle_coverage() {
        let zones = [
            zone(0.1, -0.1, 0.45, 0.23, 0.57, false),
            zone(-0.5, 0.13, 0.33, 0.09, -1.17, false),
            zone(0.0, 0.0, 1.0, 0.6, 0.0, true),
            zone(0.15, 0.2, 0.12, 0.21, 1.32, true),
        ];
        let (width, height, aspect) = (129, 75, 16. / 9.);
        let mut masks = Masks::default();
        masks.render_displaced(width * 8, height * 8, aspect, &zones, 0.);
        for y in 0..height {
            for x in 0..width {
                let p = Vector::new((x as f32 + 0.5) * 2. / width as f32 - 1., ((y as f32 + 0.5) * 2. / height as f32 - 1.) / aspect);
                let mut normal = false;
                let mut subtract_count = 0_usize;
                for z in &zones {
                    let local = nalgebra::Rotation2::new(-z.angle) * (p - z.center);
                    if local.x.abs() <= z.half.x && local.y.abs() <= z.half.y {
                        if z.invert {
                            subtract_count += 1;
                        } else {
                            normal = true;
                        }
                    }
                }
                assert_eq!(masks.sources_rgba[(y * width + x) * 4] != 0, normal, "normal ({x}, {y})");
                assert_eq!(masks.sources_rgba[(y * width + x) * 4 + 1], [0, 26, 52][subtract_count], "subtract ({x}, {y})");
            }
        }
    }

    /// 外侧描边只出现在边界上，不与内部填充打架；两个重叠块之间不该出现描边。
    #[test]
    fn exterior_rings_leave_fill_and_union_interiors_unchanged() {
        let mut masks = Masks::default();
        masks.render_displaced(800, 800, 1., &[zone(-0.2, 0., 0.3, 0.3, 0., false), zone(0.2, 0., 0.3, 0.3, 0., false)], 1.);
        let p = (100 * masks.width + 100) * 4;
        assert_eq!(&masks.rgba[p..p + 4], &[255, 0, 0, 0]);
        for p in masks.rgba.chunks_exact(4) {
            if p[0] == 255 {
                assert_eq!(&p[1..3], &[0, 0]);
            }
            if p[1] == 255 {
                assert_eq!(p[0], 0);
                assert!(p[2] > 0);
            }
        }
        assert!(masks.rgba.chunks_exact(4).any(|p| p[1] == 255));
    }

    /// 两个完全重合的 subtract 块互相抵消，不留幻影边；区域数量无上限。
    #[test]
    fn subtract_cancellation_has_no_phantom_edges_and_zone_count_is_unbounded() {
        let mut masks = Masks::default();
        masks.render_displaced(800, 800, 1., &[zone(0., 0., 0.5, 0.3, 0., true), zone(0., 0., 0.5, 0.3, 0., true)], 1.);
        assert!(masks.rgba.iter().all(|&v| v == 0));
        let mut zones: Vec<_> = (0..159).map(|_| zone(2., 2., 0.1, 0.1, 0., false)).collect();
        zones.push(zone(0., 0., 0.2, 0.2, 0., false));
        masks.render_displaced(800, 800, 1., &zones, 1.);
        assert_eq!(masks.rgba[(100 * masks.width + 100) * 4], 255);
        // 同一维度 + 同一组区域 + 同一时刻：结果必须完全可复用。
        let previous = masks.rgba.clone();
        masks.render_displaced(800, 800, 1., &zones, 1.);
        assert_eq!(masks.rgba, previous);
        masks.render_displaced(520, 560, 1., &zones, 1.);
        assert_eq!((masks.width, masks.height), (130, 140));
    }
}
