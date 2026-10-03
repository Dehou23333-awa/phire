//! Phigros 4.0 `blockAreaList` —— 剧情遮挡区域（BlockArea）的解析与渲染。
//!
//! 语义逐条对齐 `phigros.apk` 里 `GameInformation.BlockArea` +
//! `PreviewBlockControl`（`Update` → `UpdateBlocksTransform` → `UpdateBlockActivation` →
//! `UpdateBlockAnimations`）+ `BlockRender` 的逆向结果，要点：
//!
//! * 归一化屏幕空间：`(0, 0)` = 左下角，`(1, 1)` = 右上角；官方 `AnchorToWorld` 就是
//!   `(a - 0.5) * (screenW, screenH)`，**各向异性**，所以旋转发生在屏幕空间；
//! * `time` 单位是**秒**，直接使用，**不做** T(拍) 换算；
//! * 事件只有 `time` 一个时间点，表示“在 `time` 时刻到达目标值”；缓动类型和枢轴 anchor
//!   都挂在**当前（较早的）**那个关键帧上，`easeTypeX` / `easeTypeY` 分轴独立，
//!   `Zero`(13) / `One`(14) 表示“不插值”；
//! * 缩放/旋转**不是**「拿插值结果绕某个 anchor 变换一次」，而是从第 0 个关键帧开始
//!   **逐帧累乘比例 / 累加角度、每一步绕该帧自己的 anchor**，最后一步才带缓动插值
//!   （见 [`eval_scale`]）；尺寸则直接等于插值出来的 scale 乘原始尺寸；
//! * 生命周期 `appear -> enable -> disable -> disappear` 对应
//!   `Ready`（很暗的白色呼吸微光）/ `Active` / `Disabled` 三段材质；
//! * `isSubtract` 方块以 **0.1** 的权重**可叠加**地画进 subtract 图，再经
//!   `SubtractBlockBlender` 的带通（`_ClampThresholdLow=0.09` / `High=0.12`）判定
//!   “恰好被一个方块盖住”，两个及以上互相抵消。
//!
//! 渲染管线：`mask -> BlockCompose(位移合成) -> EdgeMask/GlowMask(描边+辉光) -> ActiveBlock(全屏上色)`，
//! 细节见 [`gl`] 与 `../shaders/block_*.glsl`。
//!
//! `BlockAreaStyle::glitch = false` 时退化成「只要并集」：不算位移、不描边、不上火花，
//! 用扫描线梯形分解把并集直接填成纯色。

mod gl;

pub use gl::{BlockGl, BlockPlacement};

use super::Resource;
use macroquad::prelude::*;
use rustc_hash::FxHashMap;
use serde::Deserialize;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// 缓动
// ---------------------------------------------------------------------------

/// 官方 `GetEase.EaseInfos` 的长度。谱面里实际出现过的 `easeType` 恰好是 `0..=14`
/// （扫过 1057 个官方谱面：0..14 全部用到，15 从未出现）。
pub const EASE_COUNT: usize = 15;
/// 每条曲线预采样的点数（`Instantiation` 里的 `cmp #0x65`）。
const EASE_SAMPLES: usize = 101;

/// 0 = Linear。之后每三个一组是 In / Out / InOut，幂次 = 组号 + 2。
pub const EASE_LINEAR: u8 = 0;
pub const EASE_IN_QUAD: u8 = 1;
pub const EASE_OUT_QUAD: u8 = 2;
pub const EASE_IN_OUT_QUAD: u8 = 3;
pub const EASE_IN_CUBIC: u8 = 4;
pub const EASE_OUT_CUBIC: u8 = 5;
pub const EASE_IN_OUT_CUBIC: u8 = 6;
pub const EASE_IN_QUART: u8 = 7;
pub const EASE_OUT_QUART: u8 = 8;
pub const EASE_IN_OUT_QUART: u8 = 9;
pub const EASE_IN_QUINT: u8 = 10;
pub const EASE_OUT_QUINT: u8 = 11;
pub const EASE_IN_OUT_QUINT: u8 = 12;
/// 恒为 0：保持当前值，到下一个关键帧时刻才跳变。
pub const EASE_ZERO: u8 = 13;
/// 恒为 1：立刻取目标值。
pub const EASE_ONE: u8 = 14;

/// 逐条复刻 `GetEase.Instantiation()` 建表：
///
/// * `[0][i]  = i / 100`                                        —— Linear
/// * `[3g+1][i] = (i/100)^(g+2)`                                —— In*
/// * `[3g+2][i] = 1 - (1 - i/100)^(g+2)`                        —— Out*
/// * `[3g+3][i] = i < 50 ? 0.5·In[2i] : 0.5 + 0.5·In[2i-100]`   —— InOut*
/// * `[13][i] = 0`、`[14][i] = 1`                               —— Zero / One
///
/// 注意最后那条 InOut **不是**教科书里对称的 `1 - 0.5·In(2-2t)`：官方后半段是把 In
/// 直接平移到 `[0.5, 1]` 上，所以曲线整体不对称（t=0.75 时官方给 0.625，对称版给 0.875）。
/// 这里必须照抄，否则 InOut 系列的节奏全错。
fn build_ease_table() -> [[f32; EASE_SAMPLES]; EASE_COUNT] {
    let mut t = [[0f32; EASE_SAMPLES]; EASE_COUNT];
    for i in 0..EASE_SAMPLES {
        t[0][i] = i as f32 / 100.;
        t[EASE_ZERO as usize][i] = 0.;
        t[EASE_ONE as usize][i] = 1.;
    }
    for g in 0..4usize {
        let power = (g + 2) as f32;
        let (in_i, out_i, io_i) = (3 * g + 1, 3 * g + 2, 3 * g + 3);
        for i in 0..EASE_SAMPLES {
            let x = i as f32 / 100.;
            t[in_i][i] = x.powf(power);
            t[out_i][i] = 1. - (1. - x).powf(power);
        }
        // InOut 要读整条 In，必须等上面填完
        for i in 0..EASE_SAMPLES {
            t[io_i][i] = if i < 50 {
                0.5 * t[in_i][2 * i]
            } else {
                0.5 + 0.5 * t[in_i][2 * i - 100]
            };
        }
    }
    t
}

fn ease_table() -> &'static [[f32; EASE_SAMPLES]; EASE_COUNT] {
    static TABLE: OnceLock<[[f32; EASE_SAMPLES]; EASE_COUNT]> = OnceLock::new();
    TABLE.get_or_init(build_ease_table)
}

/// 求 `easeType` 在进度 `t` 上的缓动值 —— 等价于官方 `GetEase.GetEaseWithProgress`：
/// 在 101 个预采样点之间做**线性插值**，而不是套闭式公式。
///
/// 官方对越界的 `easeType` 抛异常；渲染器不该因为一张坏谱面整段崩掉，这里钳到最后一档。
pub fn ease(code: u8, t: f32) -> f32 {
    let table = ease_table();
    let row = &table[(code as usize).min(EASE_COUNT - 1)];
    let p = t.clamp(0., 1.) * 100.;
    let i = p as usize;
    if i >= 100 {
        return row[100];
    }
    row[i] + (row[i + 1] - row[i]) * (p - i as f32)
}


// ---------------------------------------------------------------------------
// 数据模型
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone, Copy, Default, Debug)]
pub struct Percentage {
    pub x: f32,
    pub y: f32,
}

impl Percentage {
    pub fn vec(self) -> Vec2 {
        vec2(self.x, self.y)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BlockAreaEvent {
    /// 归一化锚点（旋转 / 缩放中心）
    pub anchor: Vec2,
    /// 到达时刻（秒）
    pub time: f64,
    /// 分轴缓动（旋转事件只用 `x`）
    pub ease_x: u8,
    pub ease_y: u8,
    /// 旋转事件：`x` 为角度；移动事件：目标中心；缩放事件：缩放系数
    pub value: Vec2,
}

impl BlockAreaEvent {
    fn new(anchor: Vec2, time: f64, ease_x: u8, ease_y: u8, value: Vec2) -> Self {
        Self {
            anchor,
            time,
            ease_x,
            ease_y,
            value,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRotateEvent {
    #[serde(default)]
    anchor: Percentage,
    time: f64,
    #[serde(default)]
    ease_type: u8,
    #[serde(default)]
    rotation: f32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMoveEvent {
    #[serde(default)]
    end_position: Percentage,
    time: f64,
    #[serde(default)]
    ease_type_x: u8,
    #[serde(default)]
    ease_type_y: u8,
}

fn one() -> Percentage {
    Percentage { x: 1., y: 1. }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawScaleEvent {
    #[serde(default)]
    anchor: Percentage,
    time: f64,
    #[serde(default)]
    ease_type_x: u8,
    #[serde(default)]
    ease_type_y: u8,
    #[serde(default = "one")]
    scale: Percentage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawBlockArea {
    top_right_percentage: Percentage,
    bottom_left_percentage: Percentage,
    appear_time: f64,
    enable_time: f64,
    disable_time: f64,
    disappear_time: f64,
    /// `false` = Normal，`true` = Subtract（挖洞）
    #[serde(default)]
    pub is_subtract: bool,
    #[serde(default)]
    rotate_events: Vec<RawRotateEvent>,
    #[serde(default)]
    move_events: Vec<RawMoveEvent>,
    #[serde(default)]
    scale_events: Vec<RawScaleEvent>,
}

/// 4 个时间点驱动的生命周期相位。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockPhase {
    HiddenBefore,
    /// 淡入中（Ready 材质）
    Disabled,
    Active,
    /// 淡出中（Disabled 材质）
    DisabledOut,
    HiddenAfter,
}

#[derive(Clone, Debug)]
pub struct BlockArea {
    pub bottom_left: Vec2,
    pub top_right: Vec2,
    pub appear_time: f64,
    pub enable_time: f64,
    pub disable_time: f64,
    pub disappear_time: f64,
    pub is_subtract: bool,
    pub rotate_events: Vec<BlockAreaEvent>,
    pub move_events: Vec<BlockAreaEvent>,
    pub scale_events: Vec<BlockAreaEvent>,
    pub index: usize,
}

/// `t` 时刻的方块状态（归一化空间）。
#[derive(Clone, Copy, Debug)]
pub struct BlockState {
    pub center: Vec2,
    pub size: Vec2,
    pub rotation: f32,
    pub alpha: f32,
}

#[inline]
fn ratio(v: f64, span: f64) -> f32 {
    if span <= 0. {
        1.
    } else {
        (v / span).clamp(0., 1.) as f32
    }
}

impl BlockArea {
    pub fn from_raw(raw: RawBlockArea, index: usize) -> Self {
        let mut rotate_events = raw
            .rotate_events
            .into_iter()
            .map(|e| BlockAreaEvent::new(e.anchor.vec(), e.time, e.ease_type, e.ease_type, vec2(e.rotation, 0.)))
            .collect::<Vec<_>>();
        let mut move_events = raw
            .move_events
            .into_iter()
            .map(|e| BlockAreaEvent::new(vec2(0.5, 0.5), e.time, e.ease_type_x, e.ease_type_y, e.end_position.vec()))
            .collect::<Vec<_>>();
        let mut scale_events = raw
            .scale_events
            .into_iter()
            .map(|e| BlockAreaEvent::new(e.anchor.vec(), e.time, e.ease_type_x, e.ease_type_y, e.scale.vec()))
            .collect::<Vec<_>>();
        rotate_events.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));
        move_events.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));
        scale_events.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));
        Self {
            bottom_left: raw.bottom_left_percentage.vec(),
            top_right: raw.top_right_percentage.vec(),
            appear_time: raw.appear_time,
            enable_time: raw.enable_time,
            disable_time: raw.disable_time,
            disappear_time: raw.disappear_time,
            is_subtract: raw.is_subtract,
            rotate_events,
            move_events,
            scale_events,
            index,
        }
    }

    pub fn phase(&self, t: f64) -> BlockPhase {
        if t < self.appear_time {
            BlockPhase::HiddenBefore
        } else if t < self.enable_time {
            BlockPhase::Disabled
        } else if t < self.disable_time {
            BlockPhase::Active
        } else if t < self.disappear_time {
            BlockPhase::DisabledOut
        } else {
            BlockPhase::HiddenAfter
        }
    }

    pub fn alpha(&self, t: f64) -> f32 {
        if t < self.appear_time || t >= self.disappear_time {
            0.
        } else if t < self.enable_time {
            ratio(t - self.appear_time, self.enable_time - self.appear_time)
        } else if t < self.disable_time {
            1.
        } else {
            1. - ratio(t - self.disable_time, self.disappear_time - self.disable_time)
        }
    }

    /// 归一化空间下的原始矩形 `(x0, y0, x1, y1)`。
    pub fn base_rect(&self) -> (f32, f32, f32, f32) {
        (
            self.bottom_left.x.min(self.top_right.x),
            self.bottom_left.y.min(self.top_right.y),
            self.bottom_left.x.max(self.top_right.x),
            self.bottom_left.y.max(self.top_right.y),
        )
    }

    /// `t` 时刻的 `(中心, 尺寸, 旋转角, alpha)`；不在生命周期内返回 `None`。
    ///
    /// 变换顺序照抄官方 `PreviewBlockControl.UpdateBlockAnimations`：
    /// `GetBlockGeometry` → `g__UpdateScale` → `g__UpdateRotation` → `UpdateMovement`，
    /// 每帧都从 `base_rect` 重新开始。
    pub fn state(&self, t: f64, aspect: f32) -> Option<BlockState> {
        let alpha = self.alpha(t);
        if alpha <= 0. {
            return None;
        }
        let (x0, y0, x1, y1) = self.base_rect();
        let base_center = vec2((x0 + x1) / 2., (y0 + y1) / 2.);
        let base_size = vec2(x1 - x0, y1 - y0);

        let (scale, center) = eval_scale(&self.scale_events, t, base_center);
        let size = vec2(base_size.x * scale.x.abs(), base_size.y * scale.y.abs());

        let (rotation, center) = eval_rotate(&self.rotate_events, t, center, aspect);

        let mut center = center;
        if let Some(target) = eval_move(&self.move_events, t) {
            center += target - base_center;
        }

        if size.x <= 1e-6 || size.y <= 1e-6 {
            return None;
        }
        Some(BlockState {
            center,
            size,
            rotation,
            alpha,
        })
    }

    /// 归一化空间下的四个角点（逆时针，y 向上）。
    ///
    /// 旋转在屏幕空间发生，所以回到归一化空间后不是矩形而是平行四边形，
    /// 必须按角点交给 GPU 管线，不能只给「中心 + 尺寸 + 角度」。
    pub fn corners(&self, t: f64, aspect: f32) -> Option<[Vec2; 4]> {
        let st = self.state(t, aspect)?;
        let half = st.size / 2.;
        Some([
            st.center + rotate_in_screen(vec2(-half.x, -half.y), st.rotation, aspect),
            st.center + rotate_in_screen(vec2(half.x, -half.y), st.rotation, aspect),
            st.center + rotate_in_screen(vec2(half.x, half.y), st.rotation, aspect),
            st.center + rotate_in_screen(vec2(-half.x, half.y), st.rotation, aspect),
        ])
    }

    /// 交给 GPU 管线的一帧参数；不在生命周期内返回 `None`。
    ///
    /// 相位划分照 `PreviewBlockControl.UpdateBlockActivation` + `BlockPhase`
    /// （`HiddenBefore=0, Disabled=1, Ready=2, Active=3, HiddenAfter=4`）：
    ///
    /// * `Active`   = `[enable, disable)`
    /// * `Ready`    = `[enable - ready_dur, enable)`，**只是 enable 前的一小段**准备闪光
    /// * `Disabled` = 其余所有可见时间，也就是 **`[appear, enable)` 整段 + `[disable, disappear)`**
    ///
    /// 之前把 `[appear, enable)` 整段当成 Ready，于是：
    /// 1. 该段用 `_ShineColor * _ShineBrightness(0.12)` 画，几乎是黑的（官方这里是
    ///    `DisabledBlock` 的 `_FillColor * _FillOpacity` ≈ 暗红）；
    /// 2. Ready 通道不参与 subtract 归并，所以「只有 subtract 块」的遮挡区在这里直接画不出来。
    ///
    /// `ready_dur` 对应预制体上的 `disabledBlockReadyDuration`，那个值没能从 APK 里取到；
    /// 传 0 表示关掉 Ready 这一小段（`[appear, enable)` 全程按 Disabled 画）。
    pub fn placement(&self, t: f64, aspect: f32, ready_dur: f64) -> Option<BlockPlacement> {
        if t < self.appear_time || t >= self.disappear_time {
            return None;
        }
        let active = self.enable_time <= t && t < self.disable_time;
        let ready = !active && self.enable_time - ready_dur <= t && t < self.enable_time;
        let (phase, alpha) = if active {
            (1, 1.)
        } else if t < self.enable_time {
            let p = (0u8, ratio(t - self.appear_time, self.enable_time - self.appear_time));
            if ready {
                (0, p.1)
            } else {
                (2, p.1)
            }
        } else {
            (2, 1. - ratio(t - self.disable_time, self.disappear_time - self.disable_time))
        };
        if alpha <= 0.002 {
            return None;
        }
        let corners = self.corners(t, aspect)?;
        Some(BlockPlacement {
            corners,
            alpha,
            phase,
            subtract: self.is_subtract,
        })
    }

    /// 归一化空间下的轴对齐包围盒，已裁到 `[0, 1]²`（仅 `glitch = false` 的并集路径用）。
    pub fn bounds(&self, t: f64, aspect: f32) -> Option<(f32, f32, f32, f32)> {
        let c = self.corners(t, aspect)?;
        let min = c.iter().fold(c[0], |a, &b| a.min(b));
        let max = c.iter().fold(c[0], |a, &b| a.max(b));
        let x0 = min.x.max(0.);
        let y0 = min.y.max(0.);
        let x1 = max.x.min(1.);
        let y1 = max.y.min(1.);
        if x1 - x0 <= 1e-5 || y1 - y0 <= 1e-5 {
            return None;
        }
        Some((x0, y0, x1, y1))
    }
}

/// 官方 `FindCurrentEventIndex`：最大的 `i` 使 `events[i].time <= t`；
/// 所有关键帧都晚于 `t` 时返回 `None`（此时官方整条轨道跳过，等价于用默认值）。
fn event_index(events: &[BlockAreaEvent], t: f64) -> Option<usize> {
    let mut idx: Option<usize> = None;
    for (i, e) in events.iter().enumerate() {
        if e.time > t {
            break;
        }
        idx = Some(i);
    }
    idx
}

/// 官方 `SafeDiv(num, den)`：分母约等于 0 时返回 **1**（不是 0），
/// 于是「从 0 放大到 22」这一步的比例是 1 而不是无穷，锚点不会被甩飞。
#[inline]
fn safe_div(num: f32, den: f32) -> f32 {
    if den.abs() <= f32::EPSILON {
        1.
    } else {
        num / den
    }
}

/// 官方 `CalculateEasedProgress`：进度取**较早那个关键帧**的 `easeTypeX/Y`，分轴独立。
#[inline]
fn eased_progress(cur: &BlockAreaEvent, next: &BlockAreaEvent, t: f64) -> Vec2 {
    let span = next.time - cur.time;
    let p = if span <= 0. { 1. } else { ((t - cur.time) / span).clamp(0., 1.) as f32 };
    vec2(ease(cur.ease_x, p), ease(cur.ease_y, p))
}

/// 缩放轨道 —— 逐条对应 `PreviewBlockControl.<UpdateBlockAnimations>g__UpdateScale|24_0`。
///
/// **关键**：官方不是「拿插值出来的 scale 绕某个 anchor 缩放一次」，而是
///
/// 1. 对 `i = 0 .. index-1` 依次做 `center = anchor[i] + (center - anchor[i]) * (scale[i+1] / scale[i])`；
/// 2. 最后一步才用缓动插值：`ratio = lerp(scale[index], scale[index+1], ease) / scale[index]`，
///    绕 `anchor[index]`；
/// 3. 尺寸 = 插值出来的 scale 直接乘原始尺寸（不参与上面的链）。
///
/// 因为每一步用的都是**不同关键帧自己的** anchor，中心是穿过一串 anchor 的链式变换，
/// 只在最后一个 anchor 上缩放会得到完全不同的位置（Desultory Signals 的 #2 / #4 就是这样跑偏的）。
fn eval_scale(events: &[BlockAreaEvent], t: f64, base_center: Vec2) -> (Vec2, Vec2) {
    let Some(index) = event_index(events, t) else {
        return (Vec2::ONE, base_center);
    };
    let mut center = base_center;
    for i in 0..index {
        let (cur, next) = (&events[i], &events[i + 1]);
        let ratio = ratio_of(cur.value, next.value);
        center = cur.anchor + (center - cur.anchor) * ratio;
    }
    let cur = &events[index];
    let Some(next) = events.get(index + 1) else {
        return (cur.value, center);
    };
    let p = eased_progress(cur, next, t);
    let eased = vec2(cur.value.x + (next.value.x - cur.value.x) * p.x,
                     cur.value.y + (next.value.y - cur.value.y) * p.y);
    (eased, cur.anchor + (center - cur.anchor) * ratio_of(cur.value, eased))
}

/// 两个缩放值之间的逐轴安全比例（分母为 0 时取 1）。
#[inline]
fn ratio_of(from: Vec2, to: Vec2) -> Vec2 {
    vec2(safe_div(to.x, from.x), safe_div(to.y, from.y))
}

/// 旋转轨道 —— 对应 `<UpdateBlockAnimations>g__UpdateRotation|24_1`。
///
/// 和缩放同构：中心依次绕每个关键帧自己的 anchor 转过 `rot[i+1] - rot[i]`，
/// 最后一步转过「缓动后的绝对角 − `rot[index]`」。
/// 返回的角度是**绝对角**（官方拿它去设 `transform.localEulerAngles`），不是累加值。
fn eval_rotate(events: &[BlockAreaEvent], t: f64, base_center: Vec2, aspect: f32) -> (f32, Vec2) {
    let Some(index) = event_index(events, t) else {
        return (0., base_center);
    };
    let mut center = base_center;
    for i in 0..index {
        let (cur, next) = (&events[i], &events[i + 1]);
        center = rotate_around(cur.anchor, center, next.value.x - cur.value.x, aspect);
    }
    let cur = &events[index];
    let Some(next) = events.get(index + 1) else {
        return (cur.value.x, center);
    };
    let eased = cur.value.x + (next.value.x - cur.value.x) * eased_progress(cur, next, t).x;
    (eased, rotate_around(cur.anchor, center, eased - cur.value.x, aspect))
}

/// 移动轨道：官方 `InterpolateMoveEvent` 直接对 `endPosition` 做缓动插值，
/// 再经 `AnchorToWorld` 变世界坐标；`UpdateMovement` 把它当成**新的中心**用。
fn eval_move(events: &[BlockAreaEvent], t: f64) -> Option<Vec2> {
    let index = event_index(events, t)?;
    let cur = &events[index];
    let Some(next) = events.get(index + 1) else {
        return Some(cur.value);
    };
    let p = eased_progress(cur, next, t);
    Some(vec2(cur.value.x + (next.value.x - cur.value.x) * p.x,
              cur.value.y + (next.value.y - cur.value.y) * p.y))
}

/// 绕 `anchor` 转 `deg`。旋转发生在屏幕（像素）空间里，回到归一化空间就是斜切旋转。
fn rotate_around(anchor: Vec2, point: Vec2, deg: f32, aspect: f32) -> Vec2 {
    anchor + rotate_in_screen(point - anchor, deg, aspect)
}

/// 官方 `AnchorToWorld` 把归一化坐标按 `(screenW, screenH)` **各向异性**地放大，
/// 旋转因此发生在屏幕（像素）空间里；换算回归一化空间就是一个斜切旋转。
/// `aspect` = 遮挡层渲染目标的宽 / 高。
#[inline]
pub fn rotate_in_screen(v: Vec2, deg: f32, aspect: f32) -> Vec2 {
    let (s, c) = deg.to_radians().sin_cos();
    let a = if aspect > 1e-4 { aspect } else { 1. };
    vec2(c * v.x - (s / a) * v.y, s * a * v.x + c * v.y)
}


// ---------------------------------------------------------------------------
// 材质参数（逐条抄自 index/main.js 的 C.block，也就是 ActiveBlock.mat /
// DisabledBlock.mat / ReadyBlock.mat 的数值）
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct BlockProfile {
    /// `_DisplaceStrength`（全屏上色那趟）
    pub displace_strength: f32,
    /// `_DisplaceDirection`
    pub displace_dir: [f32; 2],
    /// `_BackgroundPixelScale`：位移图采样量化到几个屏幕像素一格
    pub pixel_scale: f32,
    /// `BlockRender.edgeSize`：EdgeMask 的膨胀趟数（每趟 1 个 effectRT 像素）
    pub edge_size: i32,
    /// `BlockRender.glowRadius`：GlowMask 的膨胀趟数上限
    pub glow_radius: i32,
    /// `BlockRender.glowWeightFalloff`：环权重按 `(R - i)^falloff` 衰减
    pub glow_falloff: f32,
    /// `BlockRender.glowPassWeightThreshold`：权重低于它的膨胀趟直接不跑
    pub glow_pass_weight_threshold: f32,

    /// 官方给 `isSubtract` 方块的权重（普通方块是 1.0）。
    /// 见 `PreviewBlockControl.UpdateBlockActivation` 里的 `isSubtract ? 0.1f : 1.0f`。
    pub sub_weight: f32,
    /// `SubtractBlockBlender` 的 `_ClampThresholdLow` / `_ClampThresholdHigh`。
    /// 遮挡度落在这个区间里才算「恰好被一个 subtract 方块盖住」，输出 1，否则输出 0 ——
    /// 于是两个及以上互相抵消。
    pub sub_threshold_low: f32,
    pub sub_threshold_high: f32,

    // BlockCompose 自带一套位移参数（决定轮廓怎么抖），和全屏那趟不是一组
    pub compose_st: [f32; 2],
    pub compose_speed: f32,
    pub compose_strength: f32,
    pub compose_strength_d: f32,

    // Unlit/ActiveBlock
    pub active_fill: [f32; 3],
    pub active_fill_op: f32,
    pub active_fill_str: f32,
    pub active_edge: [f32; 3],
    pub active_edge_op: f32,
    pub active_glow: [f32; 3],
    pub active_glow_int: f32,
    pub active_tint: [f32; 3],
    pub active_spark_op: f32,
    pub active_spark_disp: f32,
    pub active_hue: f32,
    pub active_st: [f32; 2],
    pub active_speed: f32,
    pub active_blend: f32,
    pub active_spark_st: [f32; 2],

    // Unlit/DisabledBlock
    pub disabled_fill: [f32; 3],
    pub disabled_fill_op: f32,
    pub disabled_tint: [f32; 3],
    pub disabled_spark_op: f32,
    pub disabled_spark_disp: f32,
    pub disabled_st: [f32; 2],
    pub disabled_speed: f32,
    pub disabled_spark_st: [f32; 2],

    // Unlit/ReadyBlock
    pub ready_shine: [f32; 3],
    pub ready_bright: f32,
    pub ready_speed: f32,
}

impl Default for BlockProfile {
    fn default() -> Self {
        Self {
            displace_strength: 0.15,
            displace_dir: [1., 1.],
            pixel_scale: 6.,
            // BlockRender 组件上序列化的四个值（edgeSize / glowRadius /
            // glowWeightFalloff / glowPassWeightThreshold）
            edge_size: 1,
            glow_radius: 6,
            glow_falloff: 2.65,
            glow_pass_weight_threshold: 0.01,

            sub_weight: 0.1,
            sub_threshold_low: 0.09,
            sub_threshold_high: 0.12,

            compose_st: [2.13, 1.02],
            compose_speed: 2.59,
            compose_strength: 0.1,
            compose_strength_d: 0.,

            active_fill: [0.7132075, 0.23549296, 0.23549296],
            active_fill_op: 0.667,
            active_fill_str: 0.667,
            active_edge: [1., 0.33018857, 0.33018857],
            active_edge_op: 0.8,
            active_glow: [1., 0.17924517, 0.17924517],
            active_glow_int: 0.8,
            active_tint: [1., 0.28490567, 0.28490567],
            active_spark_op: 5.69,
            active_spark_disp: 2.39,
            active_hue: 0.2,
            active_st: [0.8, 0.3],
            active_speed: 1.5,
            active_blend: 0.411,
            active_spark_st: [3., 1.2],

            disabled_fill: [0.497, 0.13766898, 0.13766898],
            disabled_fill_op: 0.4,
            disabled_tint: [0.31132078, 0.077830195, 0.077830195],
            disabled_spark_op: 3.5,
            disabled_spark_disp: 2.29,
            disabled_st: [0.5, 0.2],
            disabled_speed: 0.3,
            disabled_spark_st: [3., 1.2],

            ready_shine: [1., 1., 1.],
            ready_bright: 0.12,
            ready_speed: 37.9,
        }
    }
}

impl BlockProfile {
    pub fn displace_dir(&self) -> (f32, f32) {
        let l = (self.displace_dir[0] * self.displace_dir[0] + self.displace_dir[1] * self.displace_dir[1]).sqrt();
        let l = if l <= 1e-9 { 1. } else { l };
        (self.displace_dir[0] / l, self.displace_dir[1] / l)
    }

    /// `BlockRender.GetGlowRingWeight(passIndex, glowRadius, falloff)` 的字面翻译
    /// （libil2cpp 0x1D19750）：
    ///
    /// ```text
    /// if (R < 1) return 0;
    /// if (falloff <= 0.001) return 1 / R;
    /// sum = Σ_{k=1..R} k^falloff;
    /// if (sum <= 1e-6) return 1 / R;
    /// return (R - i)^falloff / sum;
    /// ```
    pub fn glow_ring_weight(&self, index: i32) -> f32 {
        let r = self.glow_radius;
        if r < 1 {
            return 0.;
        }
        let f = self.glow_falloff;
        if f <= 0.001 {
            return 1. / r as f32;
        }
        let sum: f32 = (1..=r).map(|k| (k as f32).powf(f)).sum();
        if sum <= 1e-6 {
            return 1. / r as f32;
        }
        ((r - index) as f32).powf(f) / sum
    }
}

// ---------------------------------------------------------------------------
// 外观参数
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct BlockAreaStyle {
    /// 整个 BlockArea 层
    pub enabled: bool,
    /// `false` = 只要并集（不位移 / 不描边 / 不火花）
    pub glitch: bool,
    /// `glitch = false` 时的纯色
    pub color: Color,
    /// 预制体上的 `disabledBlockReadyDuration`：enable 前多短的一段走 ReadyBlock。
    /// APK 里 PreviewBlockControl 序列化的值是 **0.5**（紧挨着的
    /// `disabledBlockShowDuration` 也是 0.5）。0 = 关掉 Ready。
    pub ready_duration: f64,
    pub profile: BlockProfile,
}

impl Default for BlockAreaStyle {
    fn default() -> Self {
        Self {
            enabled: true,
            glitch: true,
            color: Color::new(150. / 255., 16. / 255., 30. / 255., 0.88),
            ready_duration: 0.5,
            profile: BlockProfile::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// 并集轮廓（网格边界追踪）—— 只服务 `glitch = false` 的退路
// ---------------------------------------------------------------------------

/// 归一化矩形 `(x0, y0, x1, y1)`
pub type BlockRect = (f32, f32, f32, f32);

/// 求一组归一化矩形的并集轮廓。
///
/// 以所有矩形的左右边为格线做精确栅格化（**不是**采样近似），再收集
/// “已覆盖格 / 未覆盖格”之间的有向边（方向固定为**已覆盖格在左侧**），
/// 于是外轮廓与内部孔洞的绕向自动相反，配合非零环绕填充规则即可正确挖洞。
pub fn union_contours(rects: &[BlockRect]) -> Vec<Vec<Vec2>> {
    if rects.is_empty() {
        return Vec::new();
    }
    let eps = 1e-6;

    let mut xs: Vec<f32> = Vec::with_capacity(rects.len() * 2);
    let mut ys: Vec<f32> = Vec::with_capacity(rects.len() * 2);
    for &(x0, y0, x1, y1) in rects {
        xs.push(x0);
        xs.push(x1);
        ys.push(y0);
        ys.push(y1);
    }
    let dedup = |v: &mut Vec<f32>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v.dedup_by(|a, b| (*a - *b).abs() < eps);
    };
    dedup(&mut xs);
    dedup(&mut ys);
    if xs.len() < 2 || ys.len() < 2 {
        return Vec::new();
    }
    let nx = xs.len() - 1;
    let ny = ys.len() - 1;

    let find = |v: &[f32], t: f32| -> usize {
        match v.binary_search_by(|p| p.partial_cmp(&t).unwrap_or(std::cmp::Ordering::Equal)) {
            Ok(i) => i,
            Err(i) => i.min(v.len() - 1),
        }
    };

    let mut covered = vec![false; nx * ny];
    for &(x0, y0, x1, y1) in rects {
        let (i0, i1) = (find(&xs, x0), find(&xs, x1));
        let (j0, j1) = (find(&ys, y0), find(&ys, y1));
        for i in i0..i1.min(nx) {
            for j in j0..j1.min(ny) {
                covered[i * ny + j] = true;
            }
        }
    }

    let at = |i: i32, j: i32| -> bool {
        i >= 0 && j >= 0 && (i as usize) < nx && (j as usize) < ny && covered[i as usize * ny + j as usize]
    };

    let mut edges: Vec<(Vec2, Vec2)> = Vec::new();
    for i in 0..nx {
        for j in 0..ny {
            if !covered[i * ny + j] {
                continue;
            }
            let (x0, y0, x1, y1) = (xs[i], ys[j], xs[i + 1], ys[j + 1]);
            // 方向约定：已覆盖格始终在边的左侧（y 轴向上）
            if !at(i as i32, j as i32 - 1) {
                edges.push((vec2(x0, y0), vec2(x1, y0)));
            }
            if !at(i as i32, j as i32 + 1) {
                edges.push((vec2(x1, y1), vec2(x0, y1)));
            }
            if !at(i as i32 - 1, j as i32) {
                edges.push((vec2(x0, y1), vec2(x0, y0)));
            }
            if !at(i as i32 + 1, j as i32) {
                edges.push((vec2(x1, y0), vec2(x1, y1)));
            }
        }
    }
    if edges.is_empty() {
        return Vec::new();
    }

    let key = |p: Vec2| ((p.x * 1e5).round() as i64, (p.y * 1e5).round() as i64);
    let mut outgoing: FxHashMap<(i64, i64), Vec<usize>> = FxHashMap::default();
    for (i, (a, _)) in edges.iter().enumerate() {
        outgoing.entry(key(*a)).or_default().push(i);
    }

    let mut used = vec![false; edges.len()];
    let mut contours: Vec<Vec<Vec2>> = Vec::new();
    for start in 0..edges.len() {
        if used[start] {
            continue;
        }
        let mut poly: Vec<Vec2> = Vec::new();
        let mut cur = start;
        loop {
            used[cur] = true;
            poly.push(edges[cur].0);
            let end = edges[cur].1;
            let next = outgoing.get(&key(end)).and_then(|v| v.iter().copied().find(|&e| !used[e]));
            match next {
                Some(e) => cur = e,
                None => break,
            }
        }
        if poly.len() >= 3 {
            merge_collinear(&mut poly);
            contours.push(poly);
        }
    }
    contours
}

/// 合并共线点，得到最简多边形。
fn merge_collinear(poly: &mut Vec<Vec2>) {
    if poly.len() < 3 {
        return;
    }
    let n = poly.len();
    let mut out: Vec<Vec2> = Vec::with_capacity(n);
    for i in 0..n {
        let prev = poly[(i + n - 1) % n];
        let cur = poly[i];
        let next = poly[(i + 1) % n];
        let d0 = cur - prev;
        let d1 = next - cur;
        if (d0.x * d1.y - d0.y * d1.x).abs() > 1e-7 {
            out.push(cur);
        }
    }
    if out.len() >= 3 {
        *poly = out;
    }
}

// ---------------------------------------------------------------------------
// 扫描线梯形分解（`glitch = false` 的干净并集路径）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct SweepEdge {
    a: Vec2,
    b: Vec2,
    /// 负 x 方向穿越计 +1，正 x 方向计 -1（配合非零环绕规则）
    delta: f32,
}

#[inline]
fn edge_y_at(e: &SweepEdge, x: f32) -> f32 {
    let dx = e.b.x - e.a.x;
    if dx.abs() < 1e-9 {
        e.a.y
    } else {
        e.a.y + (e.b.y - e.a.y) * (x - e.a.x) / dx
    }
}

/// 对一组闭合轮廓做 x 方向扫描，按**非零环绕**规则输出互不重叠的梯形。
fn trapezoids(contours: &[Vec<Vec2>], out: &mut Vec<([Vec2; 3], Color)>, color: Color) {
    let mut edges: Vec<SweepEdge> = Vec::new();
    let mut xs: Vec<f32> = Vec::new();
    for c in contours {
        let n = c.len();
        if n < 3 {
            continue;
        }
        for i in 0..n {
            let a = c[i];
            let b = c[(i + 1) % n];
            if (a.x - b.x).abs() <= 1e-9 {
                continue;
            }
            xs.push(a.x.min(b.x));
            xs.push(a.x.max(b.x));
            edges.push(SweepEdge {
                a,
                b,
                delta: if b.x < a.x { 1. } else { -1. },
            });
        }
    }
    if edges.is_empty() {
        return;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    xs.dedup_by(|a, b| (*a - *b).abs() <= 1e-6);

    let mut crossings: Vec<(f32, usize)> = Vec::new();
    for w in xs.windows(2) {
        let (xl, xr) = (w[0], w[1]);
        if xr - xl <= 1e-7 {
            continue;
        }
        let xm = (xl + xr) * 0.5;
        crossings.clear();
        for (idx, e) in edges.iter().enumerate() {
            let (lo, hi) = (e.a.x.min(e.b.x), e.a.x.max(e.b.x));
            // 半开区间，避免顶点被重复计数
            if xm < lo || xm >= hi {
                continue;
            }
            crossings.push((edge_y_at(e, xm), idx));
        }
        if crossings.len() < 2 {
            continue;
        }
        crossings.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut winding = 0f32;
        for k in 0..crossings.len() - 1 {
            winding += edges[crossings[k].1].delta;
            if winding == 0. {
                continue;
            }
            let e0 = &edges[crossings[k].1];
            let e1 = &edges[crossings[k + 1].1];
            let p0 = vec2(xl, edge_y_at(e0, xl));
            let p1 = vec2(xr, edge_y_at(e0, xr));
            let p2 = vec2(xr, edge_y_at(e1, xr));
            let p3 = vec2(xl, edge_y_at(e1, xl));
            out.push(([p0, p1, p2], color));
            out.push(([p0, p2, p3], color));
        }
    }
}

/// 按块绘制三角形，避免单次 `geometry()` 超过 macroquad 的 drawcall buffer 容量。
fn draw_triangles(tris: &[([Vec2; 3], Color)]) {
    const MESH_CHUNK: usize = 1002;
    if tris.is_empty() {
        return;
    }
    let mut verts: Vec<Vertex> = Vec::with_capacity(MESH_CHUNK);
    let mut indices: Vec<u16> = Vec::with_capacity(MESH_CHUNK);
    let mut gl = unsafe { get_internal_gl() };
    let gl = &mut gl.quad_gl;
    gl.texture(None);
    gl.draw_mode(DrawMode::Triangles);
    for (t, c) in tris {
        if verts.len() + 3 > MESH_CHUNK {
            gl.geometry(&verts, &indices);
            verts.clear();
            indices.clear();
        }
        let i = verts.len() as u16;
        for p in t {
            verts.push(Vertex::new(p.x, p.y, 0., 0., 0., *c));
        }
        indices.extend_from_slice(&[i, i + 1, i + 2]);
    }
    if !verts.is_empty() {
        gl.geometry(&verts, &indices);
    }
}

// ---------------------------------------------------------------------------
// 渲染器
// ---------------------------------------------------------------------------

pub struct BlockAreaRenderer {
    pub style: BlockAreaStyle,
    gl: Option<BlockGl>,
    gl_failed: bool,
    placements: Vec<BlockPlacement>,
    tris: Vec<([Vec2; 3], Color)>,
    rects: Vec<BlockRect>,
}

impl Default for BlockAreaRenderer {
    fn default() -> Self {
        Self {
            style: BlockAreaStyle::default(),
            gl: None,
            gl_failed: false,
            placements: Vec::new(),
            tris: Vec::new(),
            rects: Vec::new(),
        }
    }
}

impl BlockAreaRenderer {
    /// 主入口：`areas` 为谱面的 `blockAreaList`，`t` 为当前**秒**时间，
    /// `onto` 是要压上去的那张图。
    ///
    /// 官方是**整帧画完之后**再叠遮挡层：ActiveBlock 采的 `_SceneColor` 里已经含
    /// 判定线、音符和 UI（分数 / 曲名 / 暂停键 / 难度），所以这些全都被它盖住。
    /// 一手证据：官方 16:9 截图里，判定线上方的分数 / 暂停键是纯白 `(1.000,1.000,1.000)`，
    /// 而落在遮挡带里的曲名 / 难度只有 `(1.000,0.933,0.937)` —— G/B 被压掉约 6.5%。
    /// 因此要在 `GameScene` 画完 `ui()` 之后调用，并把 MSAA 已 resolve 的
    /// `chart_target.output()` 作为 `onto` 传进来。
    pub fn render(&mut self, res: &mut Resource, areas: &[BlockArea], t: f64, onto: Option<RenderTarget>) {
        if !self.style.enabled || areas.is_empty() {
            return;
        }
        let aspect = block_aspect(res);
        let ready_dur = self.style.ready_duration;
        self.placements.clear();
        self.placements.extend(areas.iter().filter_map(|a| a.placement(t, aspect, ready_dur)));
        if self.placements.is_empty() {
            return;
        }

        if !self.style.glitch {
            self.render_union(res, areas, t, aspect);
            return;
        }

        if self.gl.is_none() && !self.gl_failed {
            self.gl = BlockGl::new();
            if self.gl.is_none() {
                self.gl_failed = true;
                warn!("block area: failed to build the GPU pipeline, layer disabled");
            }
        }
        if let Some(gl) = &mut self.gl {
            gl.render(&self.style.profile, &self.placements, t, onto);
        }
    }

    /// `glitch = false`：只填并集，不算位移、不描边、不上火花。
    fn render_union(&mut self, res: &mut Resource, areas: &[BlockArea], t: f64, aspect: f32) {
        let (half_w, half_h) = visible_extent(res);
        self.rects.clear();
        for a in areas {
            if let Some(r) = a.bounds(t, aspect) {
                self.rects.push(r);
            }
        }
        if self.rects.is_empty() {
            return;
        }
        let contours: Vec<Vec<Vec2>> = union_contours(&self.rects)
            .iter()
            .map(|poly| poly.iter().map(|p| vec2((p.x - 0.5) * 2. * half_w, (p.y - 0.5) * 2. * half_h)).collect())
            .collect();
        let mut tris = std::mem::take(&mut self.tris);
        tris.clear();
        trapezoids(&contours, &mut tris, self.style.color);
        draw_triangles(&tris);
        self.tris = tris;
    }
}

// ---------------------------------------------------------------------------
// 便捷入口
// ---------------------------------------------------------------------------

/// 从谱面 JSON 的 `blockAreaList` 字段解析。
pub fn parse_block_areas(value: Option<&serde_json::Value>) -> Vec<BlockArea> {
    let Some(serde_json::Value::Array(list)) = value else {
        return Vec::new();
    };
    list.iter()
        .enumerate()
        .filter_map(|(i, v)| serde_json::from_value::<RawBlockArea>(v.clone()).ok().map(|raw| BlockArea::from_raw(raw, i)))
        .collect()
}

/// 计算 chart 空间下可视区域的半宽 / 半高。
///
/// `Chart::render` 内部会把 y 轴翻转，且相机 `zoom = (ratio, -aspect * ratio)`，
/// 因此可视区是 `x ∈ [-1/ratio, 1/ratio]`、`y ∈ [-1/(aspect·ratio), 1/(aspect·ratio)]`。
pub fn visible_extent(res: &Resource) -> (f32, f32) {
    let ratio = res.config.chart_ratio.clamp(0.05, 8.0);
    (1. / ratio, 1. / (res.aspect_ratio.max(1e-3) * ratio))
}

/// 遮挡层所在渲染目标的宽高比。
///
/// 官方 `AnchorToWorld` 把归一化坐标按 `(screenW, screenH)` 各向异性地放大，旋转在屏幕
/// 空间里发生，所以回归一化空间必须知道这个比例（见 [`rotate_in_screen`]）。
pub fn block_aspect(res: &Resource) -> f32 {
    let msaa = res.config.sample_count > 1;
    let rt = res
        .chart_target
        .as_ref()
        .map(|it| if msaa { it.input() } else { it.output() })
        .or_else(|| res.camera.render_target.clone());
    match rt {
        Some(rt) if rt.texture.height() > 1e-4 => rt.texture.width() / rt.texture.height(),
        _ => {
            let h = screen_height();
            if h > 1e-4 {
                screen_width() / h
            } else {
                1.
            }
        }
    }
}


// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_area(poly: &[Vec2]) -> f32 {
        let n = poly.len();
        let mut s = 0.;
        for i in 0..n {
            let a = poly[i];
            let b = poly[(i + 1) % n];
            s += a.x * b.y - b.x * a.y;
        }
        s / 2.
    }

    fn total_area(contours: &[Vec<Vec2>]) -> f32 {
        contours.iter().map(|c| signed_area(c).abs()).sum()
    }

    #[test]
    fn ease_zero_one_are_not_interpolation() {
        for t in [0., 0.25, 0.5, 0.75, 1.] {
            assert_eq!(ease(EASE_ZERO, t), 0.);
            assert_eq!(ease(EASE_ONE, t), 1.);
            assert!((ease(EASE_LINEAR, t) - t).abs() < 1e-6);
        }
        // out(t) == 1 - in(1 - t)
        for code in [1u8, 4, 7, 10] {
            for t in [0., 0.13, 0.5, 0.87, 1.] {
                assert!((ease(code + 1, t) - (1. - ease(code, 1. - t))).abs() < 1e-5, "code={code} t={t}");
            }
        }
    }

    /// 锁死「哪个编号是哪条曲线」。官方 `GetEase.Instantiation` 的幂次是
    /// `组号/3 + 2`，所以 1..12 是 Quad/Cubic/Quart/Quint 四组 In-Out-InOut，
    /// **没有 Sine**。旧实现把 1/2/3 当成 Sine，于是 4/5/6 起整段错位
    /// （5=OutCubic 被算成 OutQuad，占全部槽位 10.2%）。
    #[test]
    fn ease_ids_are_quad_cubic_quart_quint() {
        // 0.5 是采样格点，LUT 线性插值退化为精确采样值，可以断等号
        assert_eq!(ease(1, 0.5), 0.25, "1 = InQuad");
        assert_eq!(ease(4, 0.5), 0.125, "4 = InCubic");
        assert_eq!(ease(7, 0.5), 0.0625, "7 = InQuart");
        assert_eq!(ease(10, 0.5), 0.03125, "10 = InQuint");
        assert_eq!(ease(5, 0.5), 0.875, "5 = OutCubic");
        assert_eq!(ease(8, 0.5), 0.9375, "8 = OutQuart");
        assert_eq!(ease(11, 0.5), 0.96875, "11 = OutQuint");

        // 整列端点幂次核对：t=0.5 时 In 组应当正好是 2^-k。
        // （旧实现这里是 0.2929 / 0.25 / 0.125 / 0.0625 —— 整体错一位，且多出 Sine 组）
        for g in 0..4u8 {
            let k = g + 2;
            assert!((ease(3 * g + 1, 0.5) - 0.5f32.powi(k as i32)).abs() < 1e-6, "In 组 g={g}");
            assert!((ease(3 * g + 2, 0.5) - (1. - 0.5f32.powi(k as i32))).abs() < 1e-6, "Out 组 g={g}");
        }

        // 12 条曲线 + Linear 都要单调、端点正确
        for code in 0..EASE_COUNT as u8 {
            assert!((ease(code, 0.) - if code == EASE_ONE { 1. } else { 0. }).abs() < 1e-6, "code={code} 起点");
            assert!((ease(code, 1.) - if code == EASE_ZERO { 0. } else { 1. }).abs() < 1e-6, "code={code} 终点");
            for i in 0..100 {
                let (a, b) = (i as f32 / 100., (i + 1) as f32 / 100.);
                assert!(ease(code, a) <= ease(code, b) + 1e-6, "code={code} 在 {a} 处不单调");
            }
        }
    }

    /// 官方 InOut 的后半段是把 In **平移**而不是镜像，所以曲线不对称。
    #[test]
    fn ease_in_out_is_not_symmetric() {
        // t=0.75: 官方 = 0.5 + 0.5*InQuad(0.5) = 0.625；对称版会给 0.875
        assert_eq!(ease(EASE_IN_OUT_QUAD, 0.75), 0.625);
        assert_eq!(ease(EASE_IN_OUT_CUBIC, 0.75), 0.5625);
        assert_eq!(ease(EASE_IN_OUT_QUAD, 0.25), 0.125);
    }


    #[test]
    fn union_single_rect() {
        let c = union_contours(&[(0.2, 0.2, 0.8, 0.8)]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].len(), 4);
        assert!((total_area(&c) - 0.36).abs() < 1e-4);
    }

    #[test]
    fn union_two_overlapping() {
        let c = union_contours(&[(0.1, 0.1, 0.5, 0.5), (0.3, 0.3, 0.7, 0.7)]);
        assert_eq!(c.len(), 1);
        assert!((total_area(&c) - (0.16 + 0.16 - 0.04)).abs() < 1e-4);
    }

    #[test]
    fn union_containment() {
        let c = union_contours(&[(0., 0., 1., 1.), (0.25, 0.25, 0.75, 0.75)]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].len(), 4);
        assert!((total_area(&c) - 1.).abs() < 1e-4);
    }

    #[test]
    fn union_disjoint() {
        let c = union_contours(&[(0., 0., 0.4, 0.4), (0.6, 0.6, 1., 1.)]);
        assert_eq!(c.len(), 2);
        assert!((total_area(&c) - 0.32).abs() < 1e-4);
    }

    #[test]
    fn union_l_shape() {
        let c = union_contours(&[(0., 0., 0.5, 1.), (0.5, 0., 1., 0.5)]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].len(), 6);
        assert!((total_area(&c) - 0.75).abs() < 1e-4);
    }

    #[test]
    fn union_with_hole() {
        // 回字形：外框由 4 个矩形拼成，中间留洞
        let rects = [
            (0., 0., 1., 0.2),
            (0., 0.8, 1., 1.),
            (0., 0.2, 0.2, 0.8),
            (0.8, 0.2, 1., 0.8),
        ];
        let c = union_contours(&rects);
        assert_eq!(c.len(), 2, "应当得到外轮廓 + 孔洞");
        let areas: Vec<f32> = c.iter().map(|p| signed_area(p)).collect();
        assert!(areas.iter().any(|a| *a > 0.), "缺少正绕向外轮廓: {areas:?}");
        assert!(areas.iter().any(|a| *a < 0.), "缺少负绕向孔洞: {areas:?}");
        assert!((areas.iter().sum::<f32>() - (1.0 - 0.36)).abs() < 1e-4);

        // 扫描线梯形分解后的总面积必须等于并集面积（既不重叠也不漏）
        let mut tris = Vec::new();
        trapezoids(&c, &mut tris, WHITE);
        let mut sum = 0.;
        for (t, _) in &tris {
            sum += ((t[1] - t[0]).perp_dot(t[2] - t[0])).abs() / 2.;
        }
        assert!((sum - 0.64).abs() < 1e-4, "梯形覆盖面积 {sum}");
    }

    #[test]
    fn ease_comes_from_the_earlier_keyframe() {
        // 官方 CalculateEasedProgress 用的是 events[index].easeType，不是 events[index+1] 的。
        // Zero(13) 意味着「停在 events[index] 的值，到 events[index+1] 时刻才跳变」。
        let ev = |time: f64, ease: u8, value: f32| BlockAreaEvent {
            anchor: vec2(0.5, 0.5),
            time,
            ease_x: ease,
            ease_y: ease,
            value: vec2(value, 0.),
        };
        let track = [ev(0., EASE_ZERO, 1.), ev(10., EASE_LINEAR, 2.)];
        assert_eq!(eval_move(&track, 5.).unwrap().x, 1.);
        assert_eq!(eval_move(&track, 10.).unwrap().x, 2.);

        // 反过来：缓动挂在后一个关键帧上时才会插值 —— 用来确认上面不是巧合
        let lerp = [ev(0., EASE_LINEAR, 1.), ev(10., EASE_LINEAR, 2.)];
        assert!((eval_move(&lerp, 5.).unwrap().x - 1.5).abs() < 1e-6);

        // 最后一个关键帧之后不再插值；早于第一个关键帧时官方整条轨道跳过
        assert_eq!(eval_move(&lerp, 99.).unwrap().x, 2.);
        assert!(eval_move(&lerp, -1.).is_none());
    }

    /// 官方 `g__UpdateScale` 不是「拿插值出来的 scale 绕某个 anchor 缩放一次」，
    /// 而是**逐关键帧累乘比例、每一步绕该帧自己的 anchor**，最后一步才带缓动。
    /// 两个关键帧的 anchor 不同时，两种写法给出的中心差一整段距离。
    #[test]
    fn scale_walks_through_every_keyframe_anchor() {
        let raw: RawBlockArea = serde_json::from_str(
            r#"{"topRightPercentage":{"x":0.3,"y":0.55},"bottomLeftPercentage":{"x":0.1,"y":0.45},
                "appearTime":0.0,"enableTime":0.0,"disableTime":99.0,"disappearTime":99.0,
                "isSubtract":false,"rotateEvents":[],"moveEvents":[],
                "scaleEvents":[{"anchor":{"x":0.1,"y":0.5},"time":1.0,"easeTypeX":13,"easeTypeY":13,
                                "scale":{"x":2.0,"y":1.0}},
                               {"anchor":{"x":0.9,"y":0.5},"time":2.0,"easeTypeX":13,"easeTypeY":13,
                                "scale":{"x":2.0,"y":1.0}},
                               {"anchor":{"x":0.9,"y":0.5},"time":3.0,"easeTypeX":13,"easeTypeY":13,
                                "scale":{"x":2.0,"y":1.0}}]}"#,
        )
        .unwrap();
        let b = BlockArea::from_raw(raw, 0);
        // base_center = (0.2, 0.5)，base_size = (0.2, 0.1)
        // t=2.5 -> index=1：先对 i=0 走 ratio = s[1]/s[0] = 1 绕 anchor0=(0.1,0.5)（原地不动），
        // 再走缓动步 ratio = s[1]/s[1] = 1 绕 anchor1=(0.9,0.5)（还是不动）。
        let (scale, center) = eval_scale(&b.scale_events, 2.5, vec2(0.2, 0.5));
        assert_eq!((scale.x, scale.y), (2., 1.));
        assert!((center - vec2(0.2, 0.5)).length() < 1e-6, "{center}");

        // t=3.5 -> index=2=count-1：循环跑 i=0,1
        //   i=0: ratio=1 绕 (0.1,0.5) -> 不动
        //   i=1: ratio=1 绕 (0.9,0.5) -> 不动
        let (_, center) = eval_scale(&b.scale_events, 3.5, vec2(0.2, 0.5));
        assert!((center - vec2(0.2, 0.5)).length() < 1e-6, "{center}");

        // 真正能看出「链式」的例子：Desultory Signals 的 #2，scale 1 -> 0 -> 22。
        // 0 那一步让 safe_div(22, 0) = 1，于是中心被「钉」住而不是甩飞。
        let zero_mid = [
            BlockAreaEvent::new(vec2(0.5, 0.5), 0., EASE_LINEAR, EASE_LINEAR, vec2(1., 1.)),
            BlockAreaEvent::new(vec2(0.5, 0.5), 1., EASE_LINEAR, EASE_LINEAR, vec2(0., 1.)),
            BlockAreaEvent::new(vec2(0.4948, 0.4963), 2., EASE_ZERO, EASE_ZERO, vec2(22., 1.)),
        ];
        let (scale, center) = eval_scale(&zero_mid, 2.5, vec2(0.5, 0.5));
        assert!((scale.x - 22.).abs() < 1e-5, "{scale}");
        assert!((center - vec2(0.5, 0.5)).length() < 1e-5, "{center}");
    }

    /// 只有一个关键帧时官方**完全不移动中心**（循环 0 次、也没有缓动末步），
    /// 尺寸直接乘上去 —— 也就是绕方块自己的 bounds center 长大。
    #[test]
    fn single_scale_keyframe_keeps_the_center() {
        let raw: RawBlockArea = serde_json::from_str(
            r#"{"topRightPercentage":{"x":0.3,"y":0.5},"bottomLeftPercentage":{"x":0.1,"y":0.4},
                "appearTime":0.0,"enableTime":0.0,"disableTime":9.0,"disappearTime":9.0,
                "isSubtract":false,
                "rotateEvents":[],"moveEvents":[],
                "scaleEvents":[{"anchor":{"x":0.1,"y":0.45},"time":1.0,"easeTypeX":13,"easeTypeY":13,
                                "scale":{"x":6.0,"y":1.0}}]}"#,
        )
        .unwrap();
        let b = BlockArea::from_raw(raw, 0);
        let c = b.corners(5., 1.).unwrap();
        let min = c.iter().fold(c[0], |a, &v| a.min(v));
        let max = c.iter().fold(c[0], |a, &v| a.max(v));
        assert!((min.x - (-0.4)).abs() < 1e-5, "绕自身中心放大 6 倍: {min}");
        assert!((max.x - 0.8).abs() < 1e-5, "{max}");
    }

    #[test]
    fn rotation_is_absolute_and_walks_anchors() {
        let ev = |anchor: Vec2, time: f64, deg: f32| BlockAreaEvent {
            anchor,
            time,
            ease_x: EASE_LINEAR,
            ease_y: EASE_LINEAR,
            value: vec2(deg, 0.),
        };
        // 单个关键帧：官方只取绝对角，中心不动
        let one = [ev(vec2(0., 0.), 0., 30.)];
        let (deg, center) = eval_rotate(&one, 5., vec2(0.5, 0.5), 1.);
        assert_eq!(deg, 30.);
        assert!((center - vec2(0.5, 0.5)).length() < 1e-6, "{center}");

        // 两个关键帧：末步绕 anchor0 转过 (eased - rot0)，返回的角是绝对角
        let two = [ev(vec2(0.5, 0.), 0., 0.), ev(vec2(0., 0.), 10., 90.)];
        let (deg, center) = eval_rotate(&two, 5., vec2(0.5, 1.), 1.);
        assert_eq!(deg, 45.);
        // (0.5,1) 相对 anchor (0.5,0) 是 (0,1)，转 45 度 -> (-√2/2, √2/2)
        let r = std::f32::consts::FRAC_1_SQRT_2;
        let expect = vec2(0.5 - r, r);
        assert!((center - expect).length() < 1e-5, "{center} vs {expect}");

        // 过了最后一个关键帧：只把 i=0 的 delta = 90 绕 anchor0 转一次，角度取 rot[1]
        let (deg, center) = eval_rotate(&two, 15., vec2(0.5, 1.), 1.);
        assert_eq!(deg, 90.);
        assert!((center - vec2(0.5 - 1., 0.)).length() < 1e-5, "{center}");
    }

    #[test]
    fn phase_boundaries() {
        let raw: RawBlockArea = serde_json::from_str(
            r#"{"topRightPercentage":{"x":0.8,"y":0.8},"bottomLeftPercentage":{"x":0.2,"y":0.2},
                "appearTime":1.0,"enableTime":2.0,"disableTime":4.0,"disappearTime":5.0,
                "isSubtract":false,"rotateEvents":[],"moveEvents":[],"scaleEvents":[]}"#,
        )
        .unwrap();
        let b = BlockArea::from_raw(raw, 0);
        assert!(b.placement(0.5, 1., 0.).is_none());
        // [appear, enable) 是 Disabled(2)，不是 Ready(0)
        assert_eq!(b.placement(1.5, 1., 0.).unwrap().phase, 2);
        assert_eq!(b.placement(3.0, 1., 0.).unwrap().phase, 1);
        assert_eq!(b.placement(4.5, 1., 0.).unwrap().phase, 2);
        assert!(b.placement(5.0, 1., 0.).is_none());

        // 只有 enable 前 ready_dur 那一小段才是 Ready(0)
        assert_eq!(b.placement(1.6, 1., 0.2).unwrap().phase, 2, "1.6 距 enable 还有 0.4 > 0.2");
        assert_eq!(b.placement(1.9, 1., 0.2).unwrap().phase, 0, "1.9 落在 [1.8, 2.0) 内");
        // ready_dur 覆盖整段时，[appear, enable) 全变 Ready
        assert_eq!(b.placement(1.5, 1., 1.0).unwrap().phase, 0);

        // 没有事件轨道时，四个角点就是 base_rect
        let c = b.placement(3.0, 1., 0.).unwrap().corners;
        assert!((c[0] - vec2(0.2, 0.2)).length() < 1e-5);
        assert!((c[2] - vec2(0.8, 0.8)).length() < 1e-5);
    }

    /// 官方 `DisabledBlock` 的淡入淡出靠 sprite 自己的 alpha（`vs_COLOR0.w`），
    /// 覆盖度 = |subtract - normal| 对 Disabled 通道同样成立；
    /// 而「只有 subtract 块」在 enable 之前也必须能画出来（Desultory Signals 15s 的全屏块）。
    #[test]
    fn subtract_only_block_is_visible_before_enable() {
        let raw: RawBlockArea = serde_json::from_str(
            r#"{"topRightPercentage":{"x":1.0,"y":1.0},"bottomLeftPercentage":{"x":0.0,"y":0.0},
                "appearTime":15.3,"enableTime":16.04,"disableTime":16.04,"disappearTime":16.04,
                "isSubtract":true,"rotateEvents":[],"moveEvents":[],
                "scaleEvents":[{"anchor":{"x":0.5,"y":0.5},"time":15.297,"easeTypeX":11,"easeTypeY":11,
                                "scale":{"x":0.0,"y":1.0}},
                               {"anchor":{"x":0.5,"y":0.5},"time":15.446,"easeTypeX":10,"easeTypeY":10,
                                "scale":{"x":1.0,"y":1.0}}]}"#,
        )
        .unwrap();
        let b = BlockArea::from_raw(raw, 0);
        let p = b.placement(15.9, 16. / 9., 0.).expect("全屏 subtract 块在 appear..enable 之间必须可见");
        assert_eq!(p.phase, 2, "该段必须是 Disabled 通道（走 |sub - normal| 归并）");
        assert!(p.subtract);
        // appear=15.3 / enable=16.04，t=15.9 的淡入系数 = 0.6/0.74
        assert!((p.alpha - 0.6 / 0.74).abs() < 0.02, "alpha={}", p.alpha);
        assert!(p.alpha > 0.5, "淡入中途也必须画得出来");
        // anchor 在 (0.5,0.5) == 块自身中心，scale.x=1 -> 铺满单位正方形
        assert!((p.corners[0] - vec2(0., 0.)).length() < 1e-5, "{:?}", p.corners);
        assert!((p.corners[2] - vec2(1., 1.)).length() < 1e-5);
    }

    #[test]
    fn parse_example_chart() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../example/True Home True World/IN.json");
        let Ok(text) = std::fs::read_to_string(path) else {
            eprintln!("skip: example chart not found");
            return;
        };
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        let areas = parse_block_areas(json.get("blockAreaList"));
        assert_eq!(areas.len(), 48);
        assert_eq!(areas.iter().filter(|a| a.is_subtract).count(), 4);
        assert!(areas.iter().all(|a| a.appear_time > 58. && a.disappear_time < 74.));
        assert!(areas.iter().any(|a| a.alpha(65.) > 0.));
        for a in &areas {
            for i in 0..=150 {
                let t = 58. + i as f64 * 0.1;
                let _ = a.phase(t);
                let _ = a.bounds(t, 16. / 9.);
                if let Some(p) = a.placement(t, 16. / 9., 0.) {
                    assert!(p.phase < 3);
                    assert!(p.alpha > 0. && p.alpha <= 1.);
                    for c in p.corners {
                        assert!(c.is_finite());
                    }
                }
            }
        }
    }
}
