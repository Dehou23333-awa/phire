use super::{Matrix, Point, Vector};
use nalgebra::Rotation2;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockRotateEvent {
    pub anchor: Vector,
    pub time: f64,
    pub ease: i32,
    pub rotation: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockMoveEvent {
    pub end: Vector,
    pub time: f64,
    pub ease_x: i32,
    pub ease_y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockScaleEvent {
    pub anchor: Vector,
    pub time: f64,
    pub ease_x: i32,
    pub ease_y: i32,
    pub scale: Vector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockPhase {
    Hidden,
    Disabled,
    Active,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BlockArea {
    pub top_right: Vector,
    pub bottom_left: Vector,
    pub appear_time: f64,
    pub enable_time: f64,
    pub disable_time: f64,
    pub disappear_time: f64,
    pub is_subtract: bool,
    pub rotate_events: Vec<BlockRotateEvent>,
    pub move_events: Vec<BlockMoveEvent>,
    pub scale_events: Vec<BlockScaleEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockTransform {
    pub center: Vector,
    pub size: Vector,
    pub rotation: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Zone {
    pub center: Vector,
    pub half: Vector,
    pub angle: f32,
    pub invert: bool,
    pub active: bool,
    pub ready: bool,
    pub opacity: f32,
}

impl Zone {
    pub fn from_area(area: &BlockArea, time: f64, aspect: f32) -> Option<Self> {
        let phase = area.phase(time);
        if phase == BlockPhase::Hidden {
            return None;
        }
        let tr = area.transform(time, aspect);
        let half = Vector::new(tr.size.x.abs() * 0.5, tr.size.y.abs() * 0.5);
        if half.x == 0. || half.y == 0. {
            return None;
        }
        let active = phase == BlockPhase::Active;
        let fades_in = !area.is_active(area.appear_time);
        Some(Self {
            center: tr.center,
            half,
            angle: tr.rotation.to_radians(),
            invert: area.is_subtract,
            active,
            ready: !active && area.is_ready_window(time),
            opacity: if !active && fades_in {
                ((time - area.appear_time) / BlockArea::READY_DURATION).clamp(0., 1.) as f32
            } else {
                1.
            },
        })
    }
}

pub const EASE_COUNT: usize = 15;
pub const EASE_SAMPLES: usize = 101;

pub const EASE_LINEAR: i32 = 0;
pub const EASE_HOLD: i32 = 13;
pub const EASE_JUMP: i32 = 14;

fn build_ease_table() -> [[f32; EASE_SAMPLES]; EASE_COUNT] {
    let mut table = [[0f32; EASE_SAMPLES]; EASE_COUNT];

    for n in 0..EASE_SAMPLES {
        table[EASE_LINEAR as usize][n] = n as f32 / 100.;
    }

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

    table[EASE_JUMP as usize] = [1.; EASE_SAMPLES];

    table
}

fn ease_table() -> &'static [[f32; EASE_SAMPLES]; EASE_COUNT] {
    static TABLE: OnceLock<[[f32; EASE_SAMPLES]; EASE_COUNT]> = OnceLock::new();
    TABLE.get_or_init(build_ease_table)
}

pub fn eased_progress(ease: i32, progress: f64) -> f32 {
    let table = ease_table();
    let curve = table.get(ease as usize).copied().unwrap_or(table[EASE_LINEAR as usize]);

    let scaled = (progress as f32) * 100.;
    if !scaled.is_finite() {
        return curve[0];
    }
    let index = scaled as i32;
    if index >= 100 {
        return curve[100];
    }
    if index < 0 {
        return curve[0];
    }
    let index = index as usize;
    curve[index] + (scaled - index as f32) * (curve[index + 1] - curve[index])
}

#[inline]
fn eased_between(cur: f64, next: f64, t: f64, ease: i32) -> f32 {
    if next <= cur {
        1.
    } else {
        eased_progress(ease, (t - cur) / (next - cur))
    }
}

#[inline]
fn safe_div(numerator: f32, denominator: f32) -> f32 {
    let limit = f32::max(1e-6 * denominator.abs(), f32::EPSILON * 8.);
    if denominator.abs() < limit {
        1.
    } else {
        numerator / denominator
    }
}

#[inline]
pub fn pct_to_chart(p: Vector, aspect: f32) -> Vector {
    Vector::new(2. * p.x - 1., (2. * p.y - 1.) / aspect)
}

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

#[inline]
fn scale_around_anchor(point: Vector, anchor: Vector, step: Vector) -> Vector {
    anchor + Vector::new(step.x * (point.x - anchor.x), step.y * (point.y - anchor.y))
}

#[inline]
fn rotate_around_anchor(point: Vector, anchor: Vector, delta_deg: f32) -> Vector {
    if delta_deg.abs() < f32::max(1e-6 * delta_deg.abs(), f32::EPSILON * 8.) {
        return point;
    }
    anchor + Rotation2::new(delta_deg.to_radians()) * (point - anchor)
}

impl BlockArea {
    #[inline]
    pub fn is_time_valid(&self, t: f64) -> bool {
        self.appear_time <= t && t < self.disappear_time
    }

    #[inline]
    pub fn is_active(&self, t: f64) -> bool {
        self.enable_time <= t && t < self.disable_time
    }

    pub fn phase(&self, t: f64) -> BlockPhase {
        if !self.is_time_valid(t) {
            BlockPhase::Hidden
        } else if self.is_active(t) {
            BlockPhase::Active
        } else {
            BlockPhase::Disabled
        }
    }

    pub const READY_DURATION: f64 = 0.5;

    #[inline]
    pub fn is_ready_window(&self, t: f64) -> bool {
        t < self.enable_time && t >= self.enable_time - Self::READY_DURATION
    }

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

    pub fn transform(&self, t: f64, aspect: f32) -> BlockTransform {
        let bl = pct_to_chart(self.bottom_left, aspect);
        let tr = pct_to_chart(self.top_right, aspect);
        let base_size = tr - bl;
        let base_center = (bl + tr) * 0.5;

        let scale = self.scale_at(t);
        let rotation = self.rotation_at(t);

        let mut center = base_center;
        if let Some(i) = last_index(&self.scale_events, t, |e| e.time) {
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
        if let Some(target) = self.move_target(t, aspect) {
            center += target - base_center;
        }

        BlockTransform {
            center,
            size: Vector::new((base_size.x * scale.x).abs(), (base_size.y * scale.y).abs()),
            rotation,
        }
    }

    pub fn matrix(&self, t: f64, aspect: f32) -> Matrix {
        matrix_of(&self.transform(t, aspect))
    }

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
    Matrix::new_translation(&tr.center) * Rotation2::new(tr.rotation.to_radians()).to_homogeneous() * Matrix::identity().append_nonuniform_scaling(&tr.size)
}

pub const TOUCH_INSET_LOCAL: f32 = 0.05;
pub const TOUCH_INSET_SCREEN_HEIGHT_RATIO: f32 = 0.25;

#[inline]
pub fn touch_inset_world(aspect: f32) -> f32 {
    TOUCH_INSET_LOCAL * 2. / aspect
}

#[inline]
fn inset_local(size: f32, inset_world: f32) -> f32 {
    if size.abs() < 1e-6 {
        TOUCH_INSET_SCREEN_HEIGHT_RATIO
    } else {
        (inset_world / size).abs().clamp(0., TOUCH_INSET_SCREEN_HEIGHT_RATIO)
    }
}

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

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BlockAreaFileVec2 {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BlockAreaFileRotateEvent {
    #[serde(default)]
    pub anchor: Option<BlockAreaFileVec2>,
    pub time: f64,
    #[serde(default)]
    pub ease_type: i32,
    #[serde(default)]
    pub rotation: f32,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BlockAreaFileMoveEvent {
    #[serde(default)]
    pub end_position: Option<BlockAreaFileVec2>,
    pub time: f64,
    #[serde(default)]
    pub ease_type_x: i32,
    #[serde(default)]
    pub ease_type_y: i32,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BlockAreaFileScaleEvent {
    #[serde(default)]
    pub anchor: Option<BlockAreaFileVec2>,
    pub time: f64,
    #[serde(default)]
    pub ease_type_x: i32,
    #[serde(default)]
    pub ease_type_y: i32,
    #[serde(default)]
    pub scale: Option<BlockAreaFileVec2>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BlockAreaFile {
    pub top_right_percentage: BlockAreaFileVec2,
    pub bottom_left_percentage: BlockAreaFileVec2,
    #[serde(default)]
    pub appear_time: f64,
    #[serde(default)]
    pub enable_time: f64,
    #[serde(default)]
    pub disable_time: f64,
    #[serde(default)]
    pub disappear_time: f64,
    #[serde(default)]
    pub is_subtract: bool,
    #[serde(default)]
    pub rotate_events: Vec<BlockAreaFileRotateEvent>,
    #[serde(default)]
    pub move_events: Vec<BlockAreaFileMoveEvent>,
    #[serde(default)]
    pub scale_events: Vec<BlockAreaFileScaleEvent>,
}

fn file_vec(p: BlockAreaFileVec2) -> Vector {
    Vector::new(p.x, p.y)
}

fn sorted_by_time<T>(mut events: Vec<T>, time: impl Fn(&T) -> f64) -> Vec<T> {
    events.sort_by(|a, b| time(a).partial_cmp(&time(b)).unwrap_or(std::cmp::Ordering::Equal));
    events
}

pub fn block_areas_from_file(list: Vec<BlockAreaFile>) -> Vec<BlockArea> {
    list.into_iter()
        .map(|b| {
            let bl = file_vec(b.bottom_left_percentage);
            let tr = file_vec(b.top_right_percentage);
            let center = (bl + tr) * 0.5;
            let anchor = |a: Option<BlockAreaFileVec2>| a.map(file_vec).unwrap_or(center);
            BlockArea {
                top_right: tr,
                bottom_left: bl,
                appear_time: b.appear_time,
                enable_time: b.enable_time,
                disable_time: b.disable_time,
                disappear_time: b.disappear_time,
                is_subtract: b.is_subtract,
                rotate_events: sorted_by_time(
                    b.rotate_events
                        .into_iter()
                        .map(|e| BlockRotateEvent {
                            anchor: anchor(e.anchor),
                            time: e.time,
                            ease: e.ease_type,
                            rotation: e.rotation,
                        })
                        .collect(),
                    |e| e.time,
                ),
                move_events: sorted_by_time(
                    b.move_events
                        .into_iter()
                        .map(|e| BlockMoveEvent {
                            end: e.end_position.map(file_vec).unwrap_or(center),
                            time: e.time,
                            ease_x: e.ease_type_x,
                            ease_y: e.ease_type_y,
                        })
                        .collect(),
                    |e| e.time,
                ),
                scale_events: sorted_by_time(
                    b.scale_events
                        .into_iter()
                        .map(|e| BlockScaleEvent {
                            anchor: anchor(e.anchor),
                            time: e.time,
                            ease_x: e.ease_type_x,
                            ease_y: e.ease_type_y,
                            scale: e.scale.map(file_vec).unwrap_or_else(|| Vector::new(1., 1.)),
                        })
                        .collect(),
                    |e| e.time,
                ),
            }
        })
        .collect()
}

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

    #[test]
    fn ease_table_matches_native_construction() {
        assert!(close(eased_progress(EASE_LINEAR, 0.3), 0.3));

        assert!(close(eased_progress(1, 0.5), 0.25));
        assert!(close(eased_progress(2, 0.5), 0.75));
        assert!(close(eased_progress(3, 0.5), 0.5));
        assert!(close(eased_progress(4, 0.5), 0.125));
        assert!(close(eased_progress(10, 0.5), 0.03125));

        assert!(close(eased_progress(3, 0.75), 0.875));
        assert!(close(eased_progress(3, 0.25), 0.125));
        for n in 0..=100 {
            let u = n as f64 / 100.;
            let out = eased_progress(3, u) as f64;
            let mirrored = eased_progress(3, 1. - u) as f64;
            assert!((out + mirrored - 1.).abs() < 1e-5, "InOutQuad 不对称于 u={u}: {out} vs {mirrored}");
        }

        assert!(close(eased_progress(EASE_HOLD, 0.3), 0.));
        assert!(close(eased_progress(EASE_JUMP, 0.3), 1.));
    }

    #[test]
    fn ease_lookup_interpolates_one_percent_samples() {
        let expected = 0.12f32.powi(2) + 0.3 * (0.13f32.powi(2) - 0.12f32.powi(2));
        assert!((eased_progress(1, 0.123) - expected).abs() < 1e-7);
        assert!((eased_progress(1, 0.123) - 0.123f32.powi(2)).abs() > 1e-6);
        assert!(close(eased_progress(1, 1.0), 1.));
        assert!(close(eased_progress(1, 2.0), 1.));
        assert!(close(eased_progress(1, -1.0), 0.));
    }

    #[test]
    fn safe_div_returns_one_for_near_zero_denominator() {
        assert_eq!(safe_div(5., 0.), 1.);
        assert_eq!(safe_div(5., -0.), 1.);
        assert_eq!(safe_div(5., 1e-30), 1.);
        assert_eq!(safe_div(9., 3.), 3.);
    }

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
        assert!(b.is_ready_window(1.5) && !b.is_ready_window(1.49) && !b.is_ready_window(2.));
    }

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

    #[test]
    fn no_events_keeps_defaults() {
        let b = area((0.6, 0.6), (0.4, 0.4), vec![], vec![], vec![]);
        let tr = b.transform(0.5, 2.0);
        assert!(close(tr.size.x, 0.4) && close(tr.size.y, 0.2), "{tr:?}");
        assert!(close(tr.rotation, 0.));

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
        assert!(at.center.norm() < 1e-6, "{at:?}");
        assert!(close(at.size.x, 0.8) && close(at.rotation, 45.), "{at:?}");
    }

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
        assert!(close(b.transform(1., 2.).center.x, -1.), "{:?}", b.transform(1., 2.));
        assert!(close(b.transform(2., 2.).center.x, -1.), "{:?}", b.transform(2., 2.));
        assert_eq!(
            scale_around_anchor(Vector::new(0.2, 0.3), Vector::zeros(), Vector::new(safe_div(2., 0.), safe_div(2., 0.))),
            Vector::new(0.2, 0.3)
        );
    }

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
        assert!(close(tr.center.x, 1.) && close(tr.center.y, -1.), "{tr:?}");
    }

    #[test]
    fn contains_handles_rotation_units() {
        let rotate = |rotation| {
            vec![
                BlockRotateEvent {
                    anchor: Vector::new(0.5, 0.5),
                    time: 0.,
                    ease: 0,
                    rotation: 0.,
                },
                BlockRotateEvent {
                    anchor: Vector::new(0.5, 0.5),
                    time: 10.,
                    ease: 0,
                    rotation,
                },
            ]
        };
        let aspect = 2.0;
        let b = area((0.75, 0.53), (0.25, 0.47), vec![], vec![], rotate(90.));
        let tr = b.transform(10., aspect);
        assert!(close(tr.rotation, 90.), "{tr:?}");
        assert!(b.contains(Vector::new(0., 0.), 10., aspect, 0.));
        assert!(!b.contains(Vector::new(0.3, 0.), 10., aspect, 0.), "水平方向应当出局");
        assert!(b.contains(Vector::new(0., 0.2), 10., aspect, 0.), "竖直方向应当在里面");
        let b = area((0.75, 0.53), (0.25, 0.47), vec![], vec![], rotate(0.));
        assert!(b.contains(Vector::new(0.3, 0.), 10., aspect, 0.));
        assert!(!b.contains(Vector::new(0., 0.2), 10., aspect, 0.));
    }

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
        let mut future = mk(false);
        future.enable_time = 60.;
        assert!(!block_touch_blocked(&[future], p, 50., aspect));
    }

    #[test]
    fn inset_shrinks_normal_and_expands_subtract() {
        let aspect = 2.0;
        let b = area((0.52, 0.52), (0.48, 0.48), vec![], vec![], vec![]);
        let inset = touch_inset_world(aspect);
        assert!(close(inset, 0.05));
        assert!(b.contains(Vector::new(0., 0.), 50., aspect, 0.));
        assert!(b.contains(Vector::new(0., 0.), 50., aspect, inset));
        let inside = Vector::new(0.035, 0.);
        assert!(b.contains(inside, 50., aspect, 0.));
        assert!(!b.contains(inside, 50., aspect, inset));
    }

    fn file_vec(x: f32, y: f32) -> BlockAreaFileVec2 {
        BlockAreaFileVec2 { x, y }
    }

    #[test]
    fn file_layer_maps_fields_and_sorts_events() {
        let areas = block_areas_from_file(vec![BlockAreaFile {
            top_right_percentage: file_vec(1.5, 0.8),
            bottom_left_percentage: file_vec(-0.2, 0.1),
            appear_time: 1.675,
            enable_time: 2.475,
            disable_time: 11.485,
            disappear_time: 11.935,
            is_subtract: true,
            rotate_events: vec![
                BlockAreaFileRotateEvent { anchor: Some(file_vec(0.5, 0.5)), time: 30., ease_type: 7, rotation: 90. },
                BlockAreaFileRotateEvent { anchor: Some(file_vec(0.25, 0.75)), time: 10., ease_type: 3, rotation: -45. },
            ],
            move_events: vec![
                BlockAreaFileMoveEvent { end_position: Some(file_vec(0.9, 0.9)), time: 20., ease_type_x: 1, ease_type_y: 2 },
                BlockAreaFileMoveEvent { end_position: Some(file_vec(0.1, 0.2)), time: 5., ease_type_x: 4, ease_type_y: 5 },
            ],
            scale_events: vec![BlockAreaFileScaleEvent {
                anchor: None,
                time: 15.,
                ease_type_x: 6,
                ease_type_y: 8,
                scale: Some(file_vec(2., -3.)),
            }],
        }]);
        let a = &areas[0];
        assert!(close(a.top_right.x, 1.5));
        assert!(close(a.bottom_left.y, 0.1));
        assert!(a.is_subtract);
        assert!(a.is_time_valid(1.7) && !a.is_time_valid(11.94));
        assert!(a.is_active(3.) && !a.is_active(11.5));
        assert_eq!(a.rotate_events.iter().map(|e| e.time).collect::<Vec<_>>(), vec![10., 30.]);
        assert_eq!(a.move_events.iter().map(|e| e.time).collect::<Vec<_>>(), vec![5., 20.]);
        assert_eq!(a.rotate_events[0].ease, 3);
        assert!(close(a.rotate_events[0].rotation, -45.));
        assert!(close(a.move_events[0].end.x, 0.1));
        assert_eq!(a.move_events[0].ease_x, 4);
        assert_eq!(a.move_events[0].ease_y, 5);
        assert_eq!(a.scale_events[0].ease_x, 6);
        assert_eq!(a.scale_events[0].ease_y, 8);
        assert!(close(a.scale_events[0].scale.y, -3.));
        assert!(close(a.scale_events[0].anchor.x, 0.65));
    }

    #[test]
    fn file_layer_accepts_missing_fields() {
        let raw: Vec<BlockAreaFile> = serde_json::from_str(
            r#"[{"topRightPercentage":{"x":1.0,"y":1.0},"bottomLeftPercentage":{"x":0.0,"y":0.0}}]"#,
        )
        .unwrap();
        let areas = block_areas_from_file(raw);
        let a = &areas[0];
        assert!(close(a.top_right.x, 1.));
        assert!(!a.is_subtract);
        assert!(a.rotate_events.is_empty() && a.move_events.is_empty() && a.scale_events.is_empty());
        assert!(!a.is_time_valid(0.) && !a.is_time_valid(1e9));
    }
}
