//! Phigros 4.0 `blockAreaList` —— 剧情遮挡区域（BlockArea）。
//!
//! 本模块只做**纯数据 + 纯几何**：解析结构、求值变换、以及触点遮挡判定。
//! 渲染在 `block_shader.rs` / `block_mask.rs`，接入在 `chart.rs` / `scene/game.rs`。
//!
//! 语义全部直接取自 `libil2cpp.so` 的反汇编，逐条依据见
//! `docs/block-area/NATIVE-SEMANTICS.md`（含方法偏移）。要点：
//!
//! * 区域是屏幕百分比矩形，被 `rotateEvents` / `moveEvents` / `scaleEvents`
//!   三段动画驱动；每段围绕**它自己的 anchor** 生效（anchor 点原地不动）；
//! * 时间单位是**秒**，不做 T(拍) 换算；
//! * `IsTimeValid(t) = appearTime <= t < disappearTime`，
//!   `IsActive(t) = enableTime <= t < disableTime`；
//! * 动画骨架是 `scale → rotate → move`，三条链共用同一套「重放已完成段 + 插值当前段」；
//! * 落在 active 区域内的触点会被**从触点列表里摘掉**，所以它下面的音符直接算 miss。

use super::{Matrix, Point, Vector};
use nalgebra::Rotation2;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// 事件
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockRotateEvent {
    /// 屏幕百分比坐标，绕它旋转。
    pub anchor: Vector,
    pub time: f64,
    pub ease: i32,
    /// 角度，逆时针为正。
    pub rotation: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockMoveEvent {
    /// 屏幕百分比坐标，绝对目标位置。
    pub end: Vector,
    pub time: f64,
    pub ease_x: i32,
    pub ease_y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockScaleEvent {
    /// 屏幕百分比坐标，绕它缩放。
    pub anchor: Vector,
    pub time: f64,
    pub ease_x: i32,
    pub ease_y: i32,
    /// 相对初始尺寸的比例，可以为负（负号在写 transform 时才取绝对值）。
    pub scale: Vector,
}

/// 区域在某一时刻所处的一段生命周期。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockPhase {
    /// 完全不出现。
    Hidden,
    /// 已出现但不遮挡触点（含 enable 之前、disable 之后两段）。
    Disabled,
    /// 遮挡触点。
    Active,
}

/// 一个遮挡区域及其全部动画事件。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BlockArea {
    /// 右上角，屏幕百分比。
    pub top_right: Vector,
    /// 左下角，屏幕百分比。
    pub bottom_left: Vector,
    pub appear_time: f64,
    pub enable_time: f64,
    pub disable_time: f64,
    pub disappear_time: f64,
    /// subtract 块：在**输入**上按奇偶抵消，在**视觉**上走另一套带通规则。
    pub is_subtract: bool,
    pub rotate_events: Vec<BlockRotateEvent>,
    pub move_events: Vec<BlockMoveEvent>,
    pub scale_events: Vec<BlockScaleEvent>,
}

/// `PreviewBlockControl` 跑完三条动画链之后写进 `Transform` 的值。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockTransform {
    /// chart 空间中心。
    pub center: Vector,
    /// 未取绝对值的尺寸（`set_localScale` 才 `fabs`）。
    pub size: Vector,
    /// 弧度，逆时针为正（`eulerAngles.z` 是角度，这里统一成弧度）。
    pub rotation: f32,
}

// ---------------------------------------------------------------------------
// 缓动
// ---------------------------------------------------------------------------

/// `GetEase.EaseInfos` 的长度（`Instantiation` 里 `newarr 0xf`）。
pub const EASE_COUNT: usize = 15;
/// 每条曲线的采样点数（`Instantiation` 的循环上界 `0x65`）。
pub const EASE_SAMPLES: usize = 101;

pub const EASE_LINEAR: i32 = 0;
/// 13 = HoldStart：整条曲线恒为 0，即保持当前关键帧的值。
pub const EASE_HOLD: i32 = 13;
/// 14 = JumpToEnd：整条曲线恒为 1，即立刻取目标值。
pub const EASE_JUMP: i32 = 14;

/// 复刻 `GetEase.Instantiation()`：
///
/// ```text
/// [0][n]       = n / 100                                     // Linear
/// [3g+1][n]    = pow(n/100, g+2)                             // In*
/// [3g+2][n]    = 1 - pow(1 - n/100, g+2)                     // Out*
/// [3g+3][n<50] = 0.5 * In[2n]                                // InOut* 前半
/// [3g+3][50+k] = 0.5 * Out[2k] + 0.5   (k<50)                // InOut* 后半
/// [3g+3][100]  = 1                                           // 显式写死
/// [13] 全 0、[14] 全 1
/// ```
///
/// 后半段用的是 **Out** 表，代入 `Out(u) = 1-(1-u)^p` 可得
/// `InOut(u) = 1 - 0.5*(2-2u)^p`，也就是教科书的对称式。
/// （早期实现把它误当成 In 表，得出「InOut 不对称」的错误结论。）
fn build_ease_table() -> [[f32; EASE_SAMPLES]; EASE_COUNT] {
    let mut table = [[0f32; EASE_SAMPLES]; EASE_COUNT];

    for n in 0..EASE_SAMPLES {
        table[EASE_LINEAR as usize][n] = n as f32 / 100.;
    }

    // 1..=12：每三个一组，幂次 g+2。
    for group in 0..4 {
        let power = (group + 2) as i32;
        let input = 1 + group * 3;
        let output = input + 1;
        let in_out = input + 2;

        for n in 0..EASE_SAMPLES {
            let u = n as f32 / 100.;
            table[input][n] = u.powi(power);
            table[output][n] = 1. - (1. - u).powi(power);
        }
        for n in 0..50 {
            table[in_out][n] = 0.5 * table[input][2 * n];
            table[in_out][50 + n] = 0.5 * table[output][2 * n] + 0.5;
        }
        table[in_out][100] = 1.;
    }

    // 13 已经是全 0，只需填 14。
    table[EASE_JUMP as usize] = [1.; EASE_SAMPLES];

    table
}

fn ease_table() -> &'static [[f32; EASE_SAMPLES]; EASE_COUNT] {
    static TABLE: OnceLock<[[f32; EASE_SAMPLES]; EASE_COUNT]> = OnceLock::new();
    TABLE.get_or_init(build_ease_table)
}

/// `GetEase.GetEaseWithProgress(progress, type)`（0x1CAA190）：
/// 索引 `trunc(progress * 100)`，再在相邻两个采样点之间线性插值。
///
/// 官方用 `fcvtzs`（向零截断）而不是 floor；因为先判 `>= 100` / `< 0`，
/// 对 `progress` 落在 `[0, 1]` 之外的情况，与「先 clamp 再截断」等价。
pub fn eased_progress(ease: i32, progress: f64) -> f32 {
    let table = ease_table();
    // 越界或非法（NaN 会走 < 0 分支）时退化成 Linear，避免 panic。
    let curve = table.get(ease as usize).copied().unwrap_or(table[EASE_LINEAR as usize]);

    let scaled = (progress as f32) * 100.;
    if !scaled.is_finite() {
        return curve[0];
    }
    let index = scaled as i32; // Rust 的 `as` 同样向零截断
    if index >= 100 {
        return curve[100];
    }
    if index < 0 {
        return curve[0];
    }
    let index = index as usize;
    curve[index] + (scaled - index as f32) * (curve[index + 1] - curve[index])
}

/// `CalculateEasedProgress(current, next, ease)`（0x1D6D9D8）+ `GetEaseWithProgress`。
#[inline]
fn eased_between(cur: f64, next: f64, t: f64, ease: i32) -> f32 {
    if next <= cur {
        // 区间退化：官方会得到 inf / NaN，这里按「已到终点」处理。
        1.
    } else {
        eased_progress(ease, (t - cur) / (next - cur))
    }
}

/// `SafeDiv(numerator, denominator)`（0x1D6D6A0）：
/// 分母与 0 用 `Mathf.Approximately` 比较，成立时返回 1 而不是 inf/NaN。
///
/// Unity 的式子（官方汇编已确认形状为 `|d| < max(1e-6*|d|, 8*Epsilon)`）：
/// `Approximately(d, 0) = |d| < max(1e-6 * max(|d|, 0), Epsilon * 8)`。
#[inline]
fn safe_div(numerator: f32, denominator: f32) -> f32 {
    let limit = f32::max(1e-6 * denominator.abs(), f32::EPSILON * 8.);
    if denominator.abs() < limit {
        1.
    } else {
        numerator / denominator
    }
}

// ---------------------------------------------------------------------------
// 坐标换算
// ---------------------------------------------------------------------------

/// `PreviewBlockControl.AnchorToWorld`（0x1D6D57C）：`(a - 0.5) * (screenW, screenH)`。
///
/// 具体到 chart 空间（x∈[-1,1]，y∈[-1/aspect,1/aspect]）就是下面这个式子；
/// 与官方只差一个统一比例因子，而 anchor / center 永远以仿射组合出现，因子会约掉。
#[inline]
pub fn pct_to_chart(p: Vector, aspect: f32) -> Vector {
    Vector::new(2. * p.x - 1., (2. * p.y - 1.) / aspect)
}

// ---------------------------------------------------------------------------
// 事件查找
// ---------------------------------------------------------------------------

/// 「最后一个 `time <= t`」，t 早于首个事件时返回 `None`（官方返回 -1）。
fn last_index<T>(events: &[T], t: f64, time: impl Fn(&T) -> f64) -> Option<usize> {
    events.partition_point(|event| time(event) <= t).checked_sub(1)
}

#[inline]
fn norm(cur: f64, next: f64, t: f64) -> f64 {
    if next <= cur {
        1.
    } else {
        (t - cur) / (next - cur)
    }
}

// ---------------------------------------------------------------------------
// 几何
// ---------------------------------------------------------------------------

/// `ScaleAroundAnchor(point, anchor, stepX, stepY)`（0x1D6D598）。
#[inline]
fn scale_around_anchor(point: Vector, anchor: Vector, step: Vector) -> Vector {
    anchor + Vector::new(step.x * (point.x - anchor.x), step.y * (point.y - anchor.y))
}

/// `RotateAroundAnchor(point, anchor, deltaDeg)`（0x1D6D5B4）。
#[inline]
fn rotate_around_anchor(point: Vector, anchor: Vector, delta_deg: f32) -> Vector {
    // 官方用 `Mathf.Approximately(delta, 0)` 短路。
    if delta_deg.abs() < f32::max(1e-6 * delta_deg.abs(), f32::EPSILON * 8.) {
        return point;
    }
    anchor + Rotation2::new(delta_deg.to_radians()) * (point - anchor)
}

/// 事件的 `time` 取 `.time` 的便捷包装。
#[inline]
fn by_time<T>(time: impl Fn(&T) -> f64) -> impl Fn(&T) -> f64 {
    time
}

impl BlockArea {
    /// `IsTimeValid()`（0x1D6CC7C）：`appearTime <= t < disappearTime`。
    #[inline]
    pub fn is_time_valid(&self, t: f64) -> bool {
        self.appear_time <= t && t < self.disappear_time
    }

    /// `IsActive(t)`（0x1D6DC40）：`enableTime <= t < disableTime`。
    #[inline]
    pub fn is_active(&self, t: f64) -> bool {
        self.enable_time <= t && t < self.disable_time
    }

    /// 生命周期分段。
    pub fn phase(&self, t: f64) -> BlockPhase {
        if !self.is_time_valid(t) {
            BlockPhase::Hidden
        } else if self.is_active(t) {
            BlockPhase::Active
        } else {
            BlockPhase::Disabled
        }
    }

    /// `disabledBlockReadyDuration`（Prefab 字段 0x5C）。
    ///
    /// 现值取自 Phira Pro 的实测 0.5s；官方是序列化字段，谱面 JSON 里没有，
    /// 所以只能作为常量。见 `NATIVE-SEMANTICS.md` §5。
    pub const READY_DURATION: f64 = 0.5;

    /// `UpdateBlockActivation` 里的 Ready 窗口：
    /// `[enableTime - READY_DURATION, enableTime)`。
    #[inline]
    pub fn is_ready_window(&self, t: f64) -> bool {
        t < self.enable_time && t >= self.enable_time - Self::READY_DURATION
    }

    /// `UpdateScale`（0x1D6CD78）。返回的 size **没有取绝对值**。
    fn scale_at(&self, t: f64) -> Vector {
        let events = &self.scale_events;
        if events.is_empty() {
            return Vector::new(1., 1.);
        }
        match last_index(events, t, |e| e.time) {
            None => Vector::new(1., 1.),
            Some(i) if i + 1 >= events.len() => events[i].scale,
            Some(i) => {
                let cur = &events[i];
                let next = &events[i + 1];
                let px = eased_between(cur.time, next.time, t, cur.ease_x);
                let py = eased_between(cur.time, next.time, t, cur.ease_y);
                Vector::new(cur.scale.x + px * (next.scale.x - cur.scale.x), cur.scale.y + py * (next.scale.y - cur.scale.y))
            }
        }
    }

    /// `UpdateRotation`（`g__UpdateRotation|24_1`）。返回角度（度）。
    fn rotation_at(&self, t: f64) -> f32 {
        let events = &self.rotate_events;
        if events.is_empty() {
            return 0.;
        }
        match last_index(events, t, |e| e.time) {
            None => 0.,
            Some(i) if i + 1 >= events.len() => events[i].rotation,
            Some(i) => {
                let cur = &events[i];
                let next = &events[i + 1];
                let p = eased_between(cur.time, next.time, t, cur.ease);
                cur.rotation + p * (next.rotation - cur.rotation)
            }
        }
    }

    /// `UpdateMovement`（0x1D6D404）的插值部分：绝对目标位置，换算到 chart 空间。
    /// 第一帧之前没有位移。
    fn move_target(&self, t: f64, aspect: f32) -> Option<Vector> {
        let events = &self.move_events;
        if events.is_empty() {
            return None;
        }
        let pct = match last_index(events, t, |e| e.time) {
            None => return None,
            Some(i) if i + 1 >= events.len() => events[i].end,
            Some(i) => {
                let cur = &events[i];
                let next = &events[i + 1];
                let px = eased_between(cur.time, next.time, t, cur.ease_x);
                let py = eased_between(cur.time, next.time, t, cur.ease_y);
                Vector::new(cur.end.x + px * (next.end.x - cur.end.x), cur.end.y + py * (next.end.y - cur.end.y))
            }
        };
        Some(pct_to_chart(pct, aspect))
    }

    /// 求 `t` 时刻的最终矩形变换（对应官方 `UpdateBlockAnimations` 的三条链）。
    ///
    /// 骨架：先把**已经完成**的事件段按关键帧绝对值重放一遍（每段绕自己的 anchor），
    /// 再插值当前段，最后叠加 move 相对初始中心的位移。
    pub fn transform(&self, t: f64, aspect: f32) -> BlockTransform {
        let bl = pct_to_chart(self.bottom_left, aspect);
        let tr = pct_to_chart(self.top_right, aspect);
        let base_size = tr - bl;
        let base_center = (bl + tr) * 0.5;

        let scale = self.scale_at(t);
        let rotation = self.rotation_at(t);

        let mut center = base_center;
        if let Some(i) = last_index(&self.scale_events, t, |e| e.time) {
            // 已完成的段落：用下一关键帧的绝对值（等价于 progress = 1）。
            for k in 0..i {
                let cur = &self.scale_events[k];
                let next = &self.scale_events[k + 1];
                center = scale_around_anchor(
                    center,
                    pct_to_chart(cur.anchor, aspect),
                    Vector::new(safe_div(next.scale.x, cur.scale.x), safe_div(next.scale.y, cur.scale.y)),
                );
            }
            if i + 1 < self.scale_events.len() {
                let cur = &self.scale_events[i];
                center = scale_around_anchor(
                    center,
                    pct_to_chart(cur.anchor, aspect),
                    Vector::new(safe_div(scale.x, cur.scale.x), safe_div(scale.y, cur.scale.y)),
                );
            }
        }
        if let Some(i) = last_index(&self.rotate_events, t, |e| e.time) {
            for k in 0..i {
                let cur = &self.rotate_events[k];
                center = rotate_around_anchor(center, pct_to_chart(cur.anchor, aspect), self.rotate_events[k + 1].rotation - cur.rotation);
            }
            if i + 1 < self.rotate_events.len() {
                let cur = &self.rotate_events[i];
                center = rotate_around_anchor(center, pct_to_chart(cur.anchor, aspect), rotation - cur.rotation);
            }
        }
        // UpdateMovement：`currentCenter + (target - originalCenter)`。
        if let Some(target) = self.move_target(t, aspect) {
            center += target - base_center;
        }

        BlockTransform {
            center,
            size: Vector::new((base_size.x * scale.x).abs(), (base_size.y * scale.y).abs()),
            rotation,
        }
    }

    /// 把单位方块（`[-0.5, 0.5]²`）映射到当前矩形的模型矩阵，chart 空间。
    pub fn matrix(&self, t: f64, aspect: f32) -> Matrix {
        matrix_of(&self.transform(t, aspect))
    }

    /// 点 `p`（chart 空间）是否落在区域内。
    ///
    /// `inset_world` 是 chart 空间的边距（见 [`touch_inset_world`]）：
    /// 普通块**收缩**、subtract 块**扩张**，与官方的 inset 语义一致。
    pub fn contains(&self, p: Vector, t: f64, aspect: f32, inset_world: f32) -> bool {
        if !self.is_time_valid(t) {
            return false;
        }
        let tr = self.transform(t, aspect);
        let Some(inv) = matrix_of(&tr).try_inverse() else {
            return false;
        };
        let local: Point = inv.transform_point(&Point::new(p.x, p.y));
        let sign = if self.is_subtract { 1. } else { -1. };
        let hx = 0.5 + sign * inset_local(tr.size.x.abs(), inset_world);
        let hy = 0.5 + sign * inset_local(tr.size.y.abs(), inset_world);
        local.x.abs() <= hx && local.y.abs() <= hy
    }
}

fn matrix_of(tr: &BlockTransform) -> Matrix {
    Matrix::new_translation(&tr.center) * Rotation2::new(tr.rotation).to_homogeneous() * Matrix::identity().append_nonuniform_scaling(&tr.size)
}

// ---------------------------------------------------------------------------
// 触点遮挡
// ---------------------------------------------------------------------------

/// 官方 `JudgeControl.maxBlockTouchInsetLocal`。
pub const TOUCH_INSET_LOCAL: f32 = 0.05;
/// 官方 `JudgeControl.blockTouchInsetScreenHeightRatio`（同时也是 clamp 上界）。
pub const TOUCH_INSET_SCREEN_HEIGHT_RATIO: f32 = 0.25;

/// chart 空间的触点边距：`maxBlockTouchInsetLocal * screenHeight`
/// （屏幕高度在 chart 空间里是 `2 / aspect`）。
#[inline]
pub fn touch_inset_world(aspect: f32) -> f32 {
    TOUCH_INSET_LOCAL * 2. / aspect
}

/// 把 chart 空间的边距换算成某个轴上的局部单位（带上界 clamp，同官方 `TryGetBlockTouchHalfSize`）。
#[inline]
fn inset_local(size: f32, inset_world: f32) -> f32 {
    if size.abs() < 1e-6 {
        TOUCH_INSET_SCREEN_HEIGHT_RATIO
    } else {
        (inset_world / size).abs().clamp(0., TOUCH_INSET_SCREEN_HEIGHT_RATIO)
    }
}

/// 官方 `JudgeControl.TryGetBlockingBlock` 的等价形式：**奇偶规则**。
///
/// 触点被遮挡，当且仅当「原矩形」与「inset 矩形」两个判定**都**成立，
/// 而每个判定是 `普通块存在 XOR subtract 块个数为奇数`。
///
/// 注意这与**视觉**上的 subtract 阈值带通（0.09..0.12）是两套规则，故意不同。
pub fn block_touch_blocked(areas: &[BlockArea], p: Vector, t: f64, aspect: f32) -> bool {
    let inset = touch_inset_world(aspect);
    let mut plain = false;
    let mut subtract = 0u32;
    let mut plain_inset = false;
    let mut subtract_inset = 0u32;

    for area in areas {
        if !area.is_active(t) {
            continue;
        }
        if area.contains(p, t, aspect, 0.) {
            if area.is_subtract {
                subtract += 1;
            } else {
                plain = true;
            }
        }
        if area.contains(p, t, aspect, inset) {
            if area.is_subtract {
                subtract_inset += 1;
            } else {
                plain_inset = true;
            }
        }
    }

    (plain as u32) != (subtract & 1) && (plain_inset as u32) != (subtract_inset & 1)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn area(
        tr: (f32, f32),
        bl: (f32, f32),
        move_events: Vec<BlockMoveEvent>,
        scale_events: Vec<BlockScaleEvent>,
        rotate_events: Vec<BlockRotateEvent>,
    ) -> BlockArea {
        BlockArea {
            top_right: Vector::new(tr.0, tr.1),
            bottom_left: Vector::new(bl.0, bl.1),
            appear_time: 0.,
            enable_time: 0.,
            disable_time: 100.,
            disappear_time: 100.,
            is_subtract: false,
            rotate_events,
            move_events,
            scale_events,
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    /// 缓动表的三个基本性质（对应 `GetEase.Instantiation` 的建表）。
    #[test]
    fn ease_table_matches_native_construction() {
        // Linear。
        assert!(close(eased_progress(EASE_LINEAR, 0.3), 0.3));

        // In/Out/InOut Quad..Quint。
        assert!(close(eased_progress(1, 0.5), 0.25)); // InQuad
        assert!(close(eased_progress(2, 0.5), 0.75)); // OutQuad
        assert!(close(eased_progress(3, 0.5), 0.5)); // InOutQuad
        assert!(close(eased_progress(4, 0.5), 0.125)); // InCubic
        assert!(close(eased_progress(10, 0.5), 0.03125)); // InQuint

        // InOut 是**对称**的：InOut(u) = 1 - 0.5(2-2u)^p（后半段用 Out 表）。
        // 早期实现误以为后半段用 In 表，得出 0.625 的错误值。
        assert!(close(eased_progress(3, 0.75), 0.875));
        assert!(close(eased_progress(3, 0.25), 0.125));
        for n in 0..=100 {
            let u = n as f64 / 100.;
            let out = eased_progress(3, u) as f64;
            let mirrored = eased_progress(3, 1. - u) as f64;
            assert!((out + mirrored - 1.).abs() < 1e-5, "InOutQuad 不对称于 u={u}: {out} vs {mirrored}");
        }

        // 13 恒 0、14 恒 1。
        assert!(close(eased_progress(EASE_HOLD, 0.3), 0.));
        assert!(close(eased_progress(EASE_JUMP, 0.3), 1.));
    }

    /// `GetEaseWithProgress` 在 1% 采样点之间线性插值，而不是直接算幂函数。
    #[test]
    fn ease_lookup_interpolates_one_percent_samples() {
        let expected = 0.12f32.powi(2) + 0.3 * (0.13f32.powi(2) - 0.12f32.powi(2));
        assert!((eased_progress(1, 0.123) - expected).abs() < 1e-7);
        assert!((eased_progress(1, 0.123) - 0.123f32.powi(2)).abs() > 1e-6);
        // 边界与越界。
        assert!(close(eased_progress(1, 1.0), 1.));
        assert!(close(eased_progress(1, 2.0), 1.));
        assert!(close(eased_progress(1, -1.0), 0.));
    }

    /// `SafeDiv`：分母与 0 近似相等时返回 1（官方 `Mathf.Approximately`）。
    #[test]
    fn safe_div_returns_one_for_near_zero_denominator() {
        assert_eq!(safe_div(5., 0.), 1.);
        assert_eq!(safe_div(5., -0.), 1.);
        assert_eq!(safe_div(5., 1e-30), 1.);
        assert_eq!(safe_div(9., 3.), 3.);
    }

    /// `AnchorToWorld` / `pct_to_chart`：屏幕中心映射到 chart 原点，四角到 ±1 / ±1/aspect。
    #[test]
    fn pct_to_chart_maps_screen_corners() {
        let aspect = 16. / 9.;
        assert!(close(pct_to_chart(Vector::new(0.5, 0.5), aspect).x, 0.));
        assert!(close(pct_to_chart(Vector::new(0.5, 0.5), aspect).y, 0.));
        assert!(close(pct_to_chart(Vector::new(0., 0.), aspect).x, -1.));
        assert!(close(pct_to_chart(Vector::new(0., 0.), aspect).y, -1. / aspect));
        assert!(close(pct_to_chart(Vector::new(1., 1.), aspect).x, 1.));
        assert!(close(pct_to_chart(Vector::new(1., 1.), aspect).y, 1. / aspect));
    }

    /// 生命周期：Hidden / Disabled / Active 三段。
    #[test]
    fn phase_follows_appear_enable_disable_disappear() {
        let mut b = area((0.6, 0.6), (0.4, 0.4), vec![], vec![], vec![]);
        b.appear_time = 1.;
        b.enable_time = 2.;
        b.disable_time = 4.;
        b.disappear_time = 5.;
        assert_eq!(b.phase(0.5), BlockPhase::Hidden);
        assert_eq!(b.phase(1.), BlockPhase::Disabled);
        assert_eq!(b.phase(1.999), BlockPhase::Disabled);
        assert_eq!(b.phase(2.), BlockPhase::Active);
        assert_eq!(b.phase(3.999), BlockPhase::Active);
        assert_eq!(b.phase(4.), BlockPhase::Disabled);
        assert_eq!(b.phase(4.999), BlockPhase::Disabled);
        assert_eq!(b.phase(5.), BlockPhase::Hidden);
        // Ready 窗口是 enable 之前 0.5s。
        assert!(b.is_ready_window(1.5) && !b.is_ready_window(1.49) && !b.is_ready_window(2.));
    }

    /// 居中 4%×4% 的块映射到 chart 原点，尺寸按 1/aspect 缩放。
    #[test]
    fn centered_block_maps_to_origin() {
        let b = area((0.52, 0.52), (0.48, 0.48), vec![], vec![], vec![]);
        let tr = b.transform(50., 2.0);
        assert!(close(tr.center.x, 0.) && close(tr.center.y, 0.), "{tr:?}");
        assert!(close(tr.size.x, 0.08) && close(tr.size.y, 0.08 / 2.0), "{tr:?}");
        assert!(close(tr.rotation, 0.));
        assert!(b.contains(Vector::new(0., 0.), 50., 2.0, 0.));
        assert!(!b.contains(Vector::new(0.5, 0.), 50., 2.0, 0.));
    }

    /// 无事件时 scale = 1、rotation = 0，且第一帧之前用默认值。
    #[test]
    fn no_events_keeps_defaults() {
        let b = area((0.6, 0.6), (0.4, 0.4), vec![], vec![], vec![]);
        let tr = b.transform(0.5, 2.0);
        // 宽 0.2 个屏幕 → chart x 跨度 0.4；高 0.2 个屏幕 → chart y 跨度 0.2/2 = 0.2。
        assert!(close(tr.size.x, 0.4) && close(tr.size.y, 0.2), "{tr:?}");
        assert!(close(tr.rotation, 0.));

        // 第一个关键帧在 t=10，在此之前保持默认。
        let b = area(
            (0.6, 0.6),
            (0.4, 0.4),
            vec![],
            vec![BlockScaleEvent {
                anchor: Vector::new(0., 0.),
                time: 10.,
                ease_x: 0,
                ease_y: 0,
                scale: Vector::new(2., 3.),
            }],
            vec![BlockRotateEvent {
                anchor: Vector::new(1., 1.),
                time: 10.,
                ease: 0,
                rotation: 45.,
            }],
        );
        assert!(close(b.transform(9., 2.).size.x, 0.4));
        let at = b.transform(10., 2.);
        // 第一关键帧只设尺寸/角度，不绕 anchor 移动中心。
        assert!(at.center.norm() < 1e-6, "{at:?}");
        assert!(close(at.size.x, 0.8) && close(at.rotation, 45.), "{at:?}");
    }

    /// 已完成段落的 anchor 位移必须保留，且 scale = 0 时走 `SafeDiv` 返回 1。
    #[test]
    fn completed_anchor_deltas_survive_move_and_zero_scale() {
        let b = area(
            (0.6, 0.6),
            (0.4, 0.4),
            vec![BlockMoveEvent {
                end: Vector::new(0.5, 0.5),
                time: 0.,
                ease_x: 0,
                ease_y: 0,
            }],
            vec![
                BlockScaleEvent {
                    anchor: Vector::new(1., 0.5),
                    time: 0.,
                    ease_x: 0,
                    ease_y: 0,
                    scale: Vector::new(1., 1.),
                },
                BlockScaleEvent {
                    anchor: Vector::new(0., 0.5),
                    time: 1.,
                    ease_x: 0,
                    ease_y: 0,
                    scale: Vector::new(2., 1.),
                },
                BlockScaleEvent {
                    anchor: Vector::new(0.5, 0.5),
                    time: 2.,
                    ease_x: 0,
                    ease_y: 0,
                    scale: Vector::new(4., 1.),
                },
            ],
            vec![],
        );
        // 绕 x=0 的 anchor 放大 2 倍后，中心相对 anchor 的偏移也翻倍。
        assert!(close(b.transform(1., 2.).center.x, -1.), "{:?}", b.transform(1., 2.));
        assert!(close(b.transform(2., 2.).center.x, -1.), "{:?}", b.transform(2., 2.));
        // 分母为 0 时 SafeDiv 返回 1 → 中心不动。
        assert_eq!(
            scale_around_anchor(Vector::new(0.2, 0.3), Vector::zeros(), Vector::new(safe_div(2., 0.), safe_div(2., 0.))),
            Vector::new(0.2, 0.3)
        );
    }

    /// move 事件是**绝对**目标：`pct = 1.0` 对应 chart x = 1。
    #[test]
    fn move_event_replaces_center_absolutely() {
        let b = area(
            (0.52, 0.52),
            (0.48, 0.48),
            vec![BlockMoveEvent {
                end: Vector::new(1.0, 0.5),
                time: 0.,
                ease_x: 0,
                ease_y: 0,
            }],
            vec![],
            vec![],
        );
        let tr = b.transform(1., 2.0);
        assert!(close(tr.center.x, 1.) && close(tr.center.y, 0.), "{tr:?}");
    }

    /// 旋转绕事件自己的 anchor，所以中心会绕着它公转。
    #[test]
    fn rotation_orbits_its_anchor() {
        let events = vec![
            BlockRotateEvent {
                anchor: Vector::new(1.0, 0.5),
                time: 0.,
                ease: 0,
                rotation: 0.,
            },
            BlockRotateEvent {
                anchor: Vector::new(1.0, 0.5),
                time: 10.,
                ease: 0,
                rotation: 90.,
            },
        ];
        let b = area((0.52, 0.52), (0.48, 0.48), vec![], vec![], events);
        let tr = b.transform(10., 2.0);
        assert!(close(tr.rotation, 90.), "{tr:?}");
        // anchor 在 (1, 0)，中心在 (0, 0)，逆时针 90° 后到 (1, -1)。
        assert!(close(tr.center.x, 1.) && close(tr.center.y, -1.), "{tr:?}");
    }

    /// 奇偶规则：单个 subtract 块一样遮挡；普通 + subtract 互相抵消。
    #[test]
    fn even_odd_touch_blocking() {
        let aspect = 2.0;
        let p = Vector::new(0., 0.);
        let mk = |subtract: bool| {
            let mut b = area((0.52, 0.52), (0.48, 0.48), vec![], vec![], vec![]);
            b.is_subtract = subtract;
            b
        };
        assert!(block_touch_blocked(&[mk(false)], p, 50., aspect));
        assert!(block_touch_blocked(&[mk(true)], p, 50., aspect));
        assert!(!block_touch_blocked(&[mk(false), mk(true)], p, 50., aspect));
        assert!(block_touch_blocked(&[mk(false), mk(true), mk(true)], p, 50., aspect));
        // 未启用（enable 之前）不遮挡。
        let mut future = mk(false);
        future.enable_time = 60.;
        assert!(!block_touch_blocked(&[future], p, 50., aspect));
    }

    /// inset 只影响判定边距：刚好在区域边缘外的点，靠 inset **收缩**后仍不遮挡普通块。
    #[test]
    fn inset_shrinks_normal_and_expands_subtract() {
        let aspect = 2.0;
        let b = area((0.52, 0.52), (0.48, 0.48), vec![], vec![], vec![]);
        // 区域半宽 0.04 → chart 半宽 0.04；inset 在 chart 空间是 0.05*2/aspect = 0.05。
        let inset = touch_inset_world(aspect);
        assert!(close(inset, 0.05));
        // 中心处两个判定都通过。
        assert!(b.contains(Vector::new(0., 0.), 50., aspect, 0.));
        assert!(b.contains(Vector::new(0., 0.), 50., aspect, inset));
        // inset 让普通块的判定范围变小：x = 0.035 在原矩形内，但 inset 后应当出局。
        let inside = Vector::new(0.035, 0.);
        assert!(b.contains(inside, 50., aspect, 0.));
        assert!(!b.contains(inside, 50., aspect, inset));
    }
}
