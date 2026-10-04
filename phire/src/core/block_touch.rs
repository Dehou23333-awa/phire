//! 官方 touch 相机的 CPU 栅格源（在 ActiveBlock 的 SDF 之前）。
//!
//! 来源：`sharedassets12.assets` 里的 `Round10_Blur4` sprite，44×44 像素，
//! PPU=100、双线性、Clamp；prefab 尺寸 11.5，相机正交高度 10。
//! 原生 Show 是 0→尺寸、Hide 是当前→0，时长 0.1 秒，线性 clamp 插值。
//! 目标 touch 相机是**点采样**的 R8，分辨率同 mask。
//!
//! 算法逐行对应 Phira Pro 的 `block_touch.rs`，这里只改了模块路径与注释。

use macroquad::prelude::Vec2;

const MAX_TOUCHES: usize = 10;
/// `Hide`/`Show` 时长（官方 0.1 秒）。
const DURATION: f32 = 0.1;
/// sprite 世界高度：44px / 100 PPU * 11.5 / 10（相机高度）。
const SPRITE_HEIGHT: f32 = 44.0 / 100.0 * 11.5 / 10.0;

#[derive(Clone, Copy, Default)]
struct Slot {
    finger: Option<u64>,
    seen: bool,
    center: Vec2,
    start_time: f32,
    start_scale: f32,
    end_scale: f32,
    scale: f32,
}

impl Slot {
    fn advance(&mut self, time: f32) {
        let t = ((time - self.start_time) / DURATION).clamp(0.0, 1.0);
        self.scale = self.start_scale + (self.end_scale - self.start_scale) * t;
    }

    fn show(&mut self, finger: u64, center: Vec2, time: f32) {
        // 原生 `Show` 即使复用一个还没播完 Hide 的槽位，也从 0 重新开始。
        self.finger = Some(finger);
        self.center = center;
        self.seen = true;
        self.start_time = time;
        self.start_scale = 0.0;
        self.end_scale = 1.0;
        self.scale = 0.0;
    }

    fn hide(&mut self, time: f32) {
        self.finger = None;
        self.start_time = time;
        self.start_scale = self.scale;
        self.end_scale = 0.0;
    }
}

pub(super) struct TouchMask {
    slots: [Slot; MAX_TOUCHES],
    source: Vec<u8>,
    width: usize,
    height: usize,
    last_time: Option<f32>,
}

impl Default for TouchMask {
    fn default() -> Self {
        let source = image::load_from_memory(include_bytes!("../../../assets/blockarea/TouchHover.png"))
            .expect("官方 TouchHover.png 缺失，见 docs/block-area/extract_official_assets.py")
            .to_rgba8();
        Self {
            slots: [Slot::default(); MAX_TOUCHES],
            width: source.width() as usize,
            height: source.height() as usize,
            source: source.into_raw(),
            last_time: None,
        }
    }
}

impl TouchMask {
    pub fn reset(&mut self) {
        self.slots.fill(Slot::default());
        self.last_time = None;
    }

    /// 位置用「左下为原点」的屏幕 UV，谱面翻转之前。
    /// 保留 finger ID：判定输入来自 HashMap，迭代顺序在相同帧之间会变。
    pub fn update_fingers(&mut self, touches: &[(u64, Vec2)], time: f32) {
        if !time.is_finite() {
            return;
        }
        if self.last_time.is_some_and(|previous| time < previous) {
            // 确定性回放可能往回 seek；原生实例在初始化时重置，而不是保留未来的缩放值。
            self.slots.fill(Slot::default());
        }
        self.last_time = Some(time);
        for slot in &mut self.slots {
            slot.advance(time);
            slot.seen = false;
        }
        for &(finger, center) in touches.iter().take(MAX_TOUCHES) {
            if !center.x.is_finite() || !center.y.is_finite() {
                continue;
            }
            if let Some(slot) = self.slots.iter_mut().find(|slot| slot.finger == Some(finger)) {
                slot.center = center;
                slot.seen = true;
            } else if let Some(slot) = self.slots.iter_mut().find(|slot| slot.finger.is_none()) {
                slot.show(finger, center, time);
            }
        }
        for slot in &mut self.slots {
            if slot.finger.is_some() && !slot.seen {
                slot.hide(time);
            }
        }
    }

    pub fn visible(&self) -> bool {
        self.slots.iter().any(|slot| slot.finger.is_some() || slot.scale > 0.0)
    }

    /// 取 touch 相机某个像素中心的 R8 值。调用方在打包进 aux 贴图时会把每个值扩成 2×2。
    pub fn sample(&self, uv: Vec2, aspect: f32) -> u8 {
        let mut result = 0.0_f32;
        for slot in &self.slots {
            if slot.scale <= 0.0 {
                continue;
            }
            let size = SPRITE_HEIGHT * slot.scale;
            let source_uv = Vec2::new((uv.x - slot.center.x) * aspect / size + 0.5, (uv.y - slot.center.y) / size + 0.5);
            if source_uv.x < 0.0 || source_uv.x > 1.0 || source_uv.y < 0.0 || source_uv.y > 1.0 {
                continue;
            }
            let (red, alpha) = self.source_sample(source_uv);
            // Sprites/Default 是 One, OneMinusSrcAlpha，并且 RGB 会乘上纹理 alpha。
            // 每次混合后量化一次 —— 对应 R8 RT。
            result = ((red * alpha + result * (1.0 - alpha)) * 255.0).round().clamp(0.0, 255.0) / 255.0;
        }
        (result * 255.0).round() as u8
    }

    fn source_sample(&self, uv: Vec2) -> (f32, f32) {
        let x = uv.x * self.width as f32 - 0.5;
        // 导出的 PNG 是自上而下，原生 sprite 纹理 UV 是 Y 向上。
        let y = (1.0 - uv.y) * self.height as f32 - 0.5;
        let x0 = x.floor() as isize;
        let y0 = y.floor() as isize;
        let fx = x - x.floor();
        let fy = y - y.floor();
        let pixel = |x: isize, y: isize, channel: usize| {
            let x = x.clamp(0, self.width as isize - 1) as usize;
            let y = y.clamp(0, self.height as isize - 1) as usize;
            self.source[(y * self.width + x) * 4 + channel] as f32 / 255.0
        };
        let bilinear = |channel| {
            let bottom = pixel(x0, y0, channel) * (1.0 - fx) + pixel(x0 + 1, y0, channel) * fx;
            let top = pixel(x0, y0 + 1, channel) * (1.0 - fx) + pixel(x0 + 1, y0 + 1, channel) * fx;
            bottom * (1.0 - fy) + top * fy
        };
        (bilinear(0), bilinear(3))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Show/Hide 用原生缩放曲线，且 Hide 完成后不可见。
    #[test]
    fn official_bitmap_show_and_hide_use_native_scale() {
        let mut touch = TouchMask::default();
        let center = Vec2::new(0.5, 0.5);
        let edge = center + Vec2::new(0.0, 0.2);
        touch.update_fingers(&[(42, center)], 0.0);
        assert_eq!(touch.sample(center, 16.0 / 9.0), 0);
        touch.update_fingers(&[(42, center)], 0.05);
        assert_eq!(touch.sample(edge, 16.0 / 9.0), 0);
        touch.update_fingers(&[(42, center)], 0.1);
        assert!(touch.sample(edge, 16.0 / 9.0) > 0);
        assert_eq!(touch.sample(center, 16.0 / 9.0), 251);
        touch.update_fingers(&[], 0.1);
        touch.update_fingers(&[], 0.15);
        assert_eq!(touch.sample(edge, 16.0 / 9.0), 0);
        touch.update_fingers(&[], 0.201);
        assert_eq!(touch.sample(center, 16.0 / 9.0), 0);
        assert!(!touch.visible());
    }

    /// 触点顺序变化不应重播 Show；重叠时按 source-over 混合。
    #[test]
    fn finger_reordering_does_not_restart_show() {
        let mut touch = TouchMask::default();
        let center = Vec2::new(0.5, 0.5);
        touch.update_fingers(&[(1, center), (2, center)], 1.0);
        touch.update_fingers(&[(2, center), (1, center)], 1.1);
        assert_eq!(touch.sample(center, 1.0), 255);
        assert_eq!(touch.slots[0].start_time, 1.0);
        assert_eq!(touch.slots[1].start_time, 1.0);
    }

    /// 正在消失的槽位被复用时从 0 重新开始。
    #[test]
    fn disappearing_slots_restart_from_zero_when_reused() {
        let mut touch = TouchMask::default();
        let center = Vec2::new(0.5, 0.5);
        touch.update_fingers(&[(1, center)], 0.0);
        touch.update_fingers(&[(1, center)], 0.1);
        touch.update_fingers(&[], 0.1);
        touch.update_fingers(&[(2, center)], 0.15);
        assert_eq!(touch.sample(center, 1.0), 0);
        touch.update_fingers(&[(2, center)], 0.25);
        assert_eq!(touch.sample(center, 1.0), 251);
    }
}
