//! BlockArea 的 GPU 管线 —— 按 `index/`（WebGL 复刻版）逐趟移植。
//!
//! 对应关系：
//!
//! | index/ | 这里 |
//! |---|---|
//! | `shader/block_cov.frag`   ≈ Unlit/BlockCompose | `block_cov.glsl` |
//! | `shader/block_ring.frag`  ≈ Unlit/EdgeMask + GlowMask | `block_ring.glsl` |
//! | `shader/block_apply.frag` ≈ Unlit/ActiveBlock/DisabledBlock/ReadyBlock | `block_apply.glsl` |
//! | `main.js` 的 `fx_blocks` | [`BlockGl::render`] |
//!
//! 与原版的差别只有两处，都是为了让它在 macroquad 里跑：
//!
//! 1. **mask 改成六张单通道 RT**。原版用 canvas2d 的 `lighten`（逐通道取 max）把方块
//!    画进两张 RGBA 贴图；这里没有自定义混合方程，改成「按覆盖度升序、不混合地画」——
//!    不混合时最后画的赢，升序即等于取 max。为了不让各通道互相覆盖，
//!    Active / Disabled / Ready 与各自的 subtract 各占一张 RT。
//! 2. **顶点直接给 NDC**（见 `block_vert.glsl`），不经过 Model / Projection。

use super::BlockProfile;
use crate::core::{copy_fbo, internal_id};
use macroquad::material::{gl_use_default_material, gl_use_material, load_material, Material, MaterialParams};
use macroquad::miniquad::{BlendFactor, BlendState, Equation, FilterMode as MqFilter, PipelineParams, ShaderSource, TextureWrap, UniformDesc, UniformType};
use macroquad::prelude::*;

const VERT: &str = include_str!("../shaders/block_vert.glsl");
const MASK: &str = include_str!("../shaders/block_mask.glsl");
const COV: &str = include_str!("../shaders/block_cov.glsl");
const RING: &str = include_str!("../shaders/block_ring.glsl");
const EFFECT: &str = include_str!("../shaders/block_effect.glsl");
const APPLY: &str = include_str!("../shaders/block_apply.glsl");
const COPY: &str = include_str!("../shaders/block_copy.glsl");

const DISP_PNG: &[u8] = include_bytes!("BlockNoise1.png");
const SPARK_PNG: &[u8] = include_bytes!("PointNoise.png");

/// 一帧里某个方块的光栅化参数。
pub struct BlockPlacement {
    /// 归一化空间下的四个角点（逆时针，y 向上）
    pub corners: [Vec2; 4],
    /// 覆盖度 = 淡入淡出系数
    pub alpha: f32,
    /// 0 = Ready / 1 = Active / 2 = Disabled
    pub phase: u8,
    pub subtract: bool,
}

// ---------------------------------------------------------------------------
// 管线
// ---------------------------------------------------------------------------

struct Targets {
    w: u32,
    h: u32,
    scene: RenderTarget,
    cov: RenderTarget,
    ring: RenderTarget,
    /// GlowMask 的 ping-pong 两张：R = 已膨胀的轮廓，G = 累积辉光
    ping_a: RenderTarget,
    ping_b: RenderTarget,
    result: RenderTarget,
    /// 下标 = phase（0 Ready / 1 Active / 2 Disabled）
    mask: [RenderTarget; 3],
    sub: [RenderTarget; 3],
}

impl Targets {
    fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            scene: render_target(w, h),
            cov: render_target(w, h),
            ring: render_target(w, h),
            ping_a: render_target(w, h),
            ping_b: render_target(w, h),
            result: render_target(w, h),
            mask: [render_target(w, h), render_target(w, h), render_target(w, h)],
            sub: [render_target(w, h), render_target(w, h), render_target(w, h)],
        }
    }
}

pub struct BlockGl {
    mask_mat: Material,
    mask_add_mat: Material,
    cov_mat: Material,
    ring_mat: Material,
    effect_mat: Material,
    apply_mat: Material,
    copy_mat: Material,
    disp: Texture2D,
    spark: Texture2D,
    targets: Targets,
    verts: Vec<Vertex>,
    idx: Vec<u16>,
}

fn internal_gl() -> macroquad::window::InternalGlContext<'static> {
    unsafe { get_internal_gl() }
}

/// 所有 pass 都是「整片覆盖」：cov / ring 会主动写 alpha = 0，
/// 开着 alpha 混合会把它们整趟丢掉，所以一律关混合。
fn no_blend() -> PipelineParams {
    PipelineParams {
        color_blend: None,
        alpha_blend: None,
        ..Default::default()
    }
}

/// subtract 图要**可叠加**：官方每个 subtract 方块贡献 0.1，交给 `SubtractBlockBlender`
/// 做「恰好一个」的带通判定。miniquad 没有 `GL_MAX` 方程，所以 normal 图用
/// 「升序 + 关混合」模拟 lighten，subtract 图改用一次加法混合。
fn additive() -> PipelineParams {
    let one = BlendState::new(Equation::Add, BlendFactor::One, BlendFactor::One);
    PipelineParams {
        color_blend: Some(one),
        alpha_blend: Some(one),
        ..Default::default()
    }
}

fn build_material(frag: &str, uniforms: &[(&str, UniformType)], textures: &[&str]) -> Option<Material> {
    build_material_with(frag, uniforms, textures, no_blend())
}

fn build_material_with(frag: &str, uniforms: &[(&str, UniformType)], textures: &[&str], pipeline_params: PipelineParams) -> Option<Material> {
    match load_material(
        ShaderSource::Glsl { vertex: VERT, fragment: frag },
        MaterialParams {
            pipeline_params,
            uniforms: uniforms.iter().map(|(n, t)| UniformDesc::new(n, *t)).collect(),
            textures: textures.iter().map(|n| (*n).to_owned()).collect(),
        },
    ) {
        Ok(m) => Some(m),
        Err(err) => {
            warn!("block area: shader build failed: {:?}", err);
            None
        }
    }
}

/// BlockNoise1.png = `_DisplaceMap`（Mirror / Point，ST=(0.8,0.3)）；
/// PointNoise.png = `_SparkMap`（Repeat / Point，ST=(3,1.2)）。
/// 两个 wrap 都取自 APK 里贴图自身的 `m_TextureSettings.m_WrapMode`
/// （Repeat=0 / Clamp=1 / Mirror=2）—— 火花图用错 wrap 会让整屏只剩边缘一列可采，
/// 结果就是遮挡区退化成一片死平的黑红。
/// 线性过滤会把 256² 的噪声抹平，边缘就从「逐像素碎裂」变成平滑扭动。
fn noise_texture(png: &[u8], wrap: TextureWrap) -> Option<Texture2D> {
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let (w, h) = (img.width() as u16, img.height() as u16);
    // 原版靠 UNPACK_FLIP_Y_WEBGL 让纹理 v=0 对应图片底边；
    // macroquad 按行号上传（v=0 = 第一行 = 图片顶边），所以这里翻一次。
    let mut data = img.into_raw();
    let stride = w as usize * 4;
    for y in 0..h as usize / 2 {
        let (a, b) = (y * stride, (h as usize - 1 - y) * stride);
        for i in 0..stride {
            data.swap(a + i, b + i);
        }
    }
    let tex = Texture2D::from_rgba8(w, h, &data);
    {
        let gl = internal_gl();
        let ctx = &mut *gl.quad_context;
        ctx.texture_set_wrap(tex.raw_miniquad_id(), wrap, wrap);
    }
    tex.set_filter(MqFilter::Nearest);
    Some(tex)
}

const F1: UniformType = UniformType::Float1;
const F2: UniformType = UniformType::Float2;
const F3: UniformType = UniformType::Float3;

impl BlockGl {
    pub fn new() -> Option<Self> {
        let mask_mat = build_material(MASK, &[], &[])?;
        let mask_add_mat = build_material_with(MASK, &[], &[], additive())?;
        let cov_mat = build_material(
            COV,
            &[
                ("u_res", F2),
                ("u_dir", F2),
                ("u_ps", F1),
                ("u_tx", F1),
                ("u_stA", F2),
                ("u_spA", F1),
                ("u_stD", F2),
                ("u_spD", F1),
                ("u_strength", F1),
                ("u_strengthD", F1),
                ("u_subLo", F1),
                ("u_subHi", F1),
            ],
            &["u_maskA", "u_maskD", "u_maskR", "u_subA", "u_subD", "u_subR", "u_disp"],
        )?;
        let ring_mat = build_material(RING, &[("u_step", F2), ("u_weight", F1), ("u_first", F1)], &["u_main", "u_cov"])?;
        let effect_mat = build_material(EFFECT, &[("u_step", F2)], &["u_glow", "u_cov"])?;
        let apply_mat = build_material(
            APPLY,
            &[
                ("u_res", F2),
                ("u_dir", F2),
                ("u_ps", F1),
                ("u_stA", F2),
                ("u_spA", F1),
                ("u_stD", F2),
                ("u_spD", F1),
                ("u_strength", F1),
                ("u_tx", F1),
                ("u_ty", F1),
                ("u_fillA", F3),
                ("u_fillOpA", F1),
                ("u_fillStrA", F1),
                ("u_edgeA", F3),
                ("u_edgeOpA", F1),
                ("u_glowA", F3),
                ("u_glowIntA", F1),
                ("u_tintA", F3),
                ("u_sparkOpA", F1),
                ("u_sparkDispA", F1),
                ("u_hueA", F1),
                ("u_sparkSTA", F2),
                ("u_dispBlendA", F1),
                ("u_fillD", F3),
                ("u_fillOpD", F1),
                ("u_tintD", F3),
                ("u_sparkOpD", F1),
                ("u_sparkDispD", F1),
                ("u_sparkSTD", F2),
                ("u_shineCol", F3),
                ("u_shineBright", F1),
                ("u_shineSpeed", F1),
            ],
            &["u_src", "u_cov", "u_ring", "u_spark", "u_disp"],
        )?;
        let disp = noise_texture(DISP_PNG, TextureWrap::Mirror)?;
        let spark = noise_texture(SPARK_PNG, TextureWrap::Repeat)?;
        let copy_mat = build_material(COPY, &[], &["u_src"])?;
        Some(Self {
            mask_mat,
            mask_add_mat,
            cov_mat,
            ring_mat,
            effect_mat,
            apply_mat,
            copy_mat,
            disp,
            spark,
            targets: Targets::new(1, 1),
            verts: Vec::with_capacity(1024),
            idx: Vec::with_capacity(1536),
        })
    }

    /// 把整条管线跑一遍：读 `onto` 当前内容 → mask → cov → ring → apply → 写回 `onto`。
    ///
    /// `onto` 由调用方给：官方把遮挡层画在**整帧（含 UI）之上**，那时 MSAA 已经
    /// resolve 完、内容在 `chart_target.output()` 里，不能再按 MSAA 去猜 `input()`。
    pub fn render(&mut self, profile: &BlockProfile, placements: &[BlockPlacement], t: f64, onto: Option<RenderTarget>) {
        // 背景必须先真的落进 FBO，否则下面 blit 出来的还是上一帧
        internal_gl().flush();

        let (w, h) = match &onto {
            Some(rt) => (rt.texture.width() as u32, rt.texture.height() as u32),
            None => (screen_width() as u32, screen_height() as u32),
        };
        if w == 0 || h == 0 {
            return;
        }
        if self.targets.w != w || self.targets.h != h {
            self.targets = Targets::new(w, h);
        }

        // 记下当前 viewport，跑完还回去（后面的判定线 / note 还要用它）。
        // 0x0BA2 = GL_VIEWPORT —— miniquad 的 gl 模块没有导出这个常量。
        let mut vp = [0i32; 4];
        unsafe { miniquad::gl::glGetIntegerv(0x0BA2, vp.as_mut_ptr()) };

        let src_fbo = onto.as_ref().map(|rt| internal_id(rt.clone())).unwrap_or(0);

        // ---- 1. 六张覆盖度 RT ----
        {
            let gl = internal_gl();
            gl.quad_gl.viewport(Some((0, 0, w as i32, h as i32)));
        }
        for phase in 0..3usize {
            for sub in 0..2usize {
                // subtract 图：每个方块按 sub_weight 叠加，交给 block_cov 做「恰好一个」判定
                let additive = sub == 1;
                let dst = if sub == 0 { self.targets.mask[phase].clone() } else { self.targets.sub[phase].clone() };
                {
                    let gl = internal_gl();
                    gl.quad_gl.render_pass(Some(dst.render_pass.raw_miniquad_id()));
                }
                // RT 不会自动清空（macroquad 用 PassAction::Nothing），不铺一层 0 的话
                // 覆盖度会跨帧累积成「历史并集」，方块消失后遮蔽还留在原地。
                self.emit_mask(&[], &[], Some(dst), 1., false);
                // Ready 不是 Disabled 的替代品：官方用 `disabledNormalReadyBlockCamera` /
                // `disabledSubtractReadyBlockCamera` 把同一批 disabled 方块**再画一遍**到
                // ready 的 RT 上，ActiveBlock 的闪烁权重是两者乘积
                // `ReadyCompose.x * |DisabledSubtract.y - DisabledNormal.x|`。
                // 所以 phase 0 的方块要同时进 Ready 图和 Disabled 图，
                // 否则进入 ready 窗口时暗红填色会整块消失。
                let mut order: Vec<usize> = (0..placements.len())
                    .filter(|i| {
                        let p = placements[*i].phase as usize;
                        let hit = match phase {
                            0 => p == 0,
                            2 => p == 0 || p == 2,
                            _ => p == phase,
                        };
                        hit && placements[*i].subtract as usize == sub
                    })
                    .collect();
                if !additive {
                    // 升序 + 关混合 → 最后画的赢 = 逐像素 max（原版 canvas 的 lighten）
                    order.sort_by(|a, b| placements[*a].alpha.partial_cmp(&placements[*b].alpha).unwrap_or(std::cmp::Ordering::Equal));
                }
                let weight = if additive { profile.sub_weight } else { 1. };
                self.emit_mask(placements, &order, None, weight, additive);
            }
        }
        gl_use_default_material();
        internal_gl().flush();

        // ---- 2. 场景拷一份出来给 apply 采样 ----
        copy_fbo(src_fbo, internal_id(self.targets.scene.clone()), (w, h));

        // ---- 3. BlockCompose ----
        self.set_compose_uniforms(profile, w, h, t);
        self.cov_mat.set_texture("u_maskA", self.targets.mask[1].texture.clone());
        self.cov_mat.set_texture("u_maskD", self.targets.mask[2].texture.clone());
        self.cov_mat.set_texture("u_maskR", self.targets.mask[0].texture.clone());
        self.cov_mat.set_texture("u_subA", self.targets.sub[1].texture.clone());
        self.cov_mat.set_texture("u_subD", self.targets.sub[2].texture.clone());
        self.cov_mat.set_texture("u_subR", self.targets.sub[0].texture.clone());
        self.cov_mat.set_texture("u_disp", self.disp.clone());
        self.pass(&self.cov_mat, &self.targets.cov);

        // ---- 4. EdgeMask + GlowMask（官方 RenderEffects 的 ping-pong）----
        let step = [1. / w as f32, 1. / h as f32];
        self.ring_mat.set_texture("u_cov", self.targets.cov.texture.clone());
        self.ring_mat.set_uniform("u_step", step);
        // 第 0 趟的种子就是 compose 本身；之后在 pingA / pingB 之间来回
        let mut src = self.targets.cov.texture.clone();
        let mut use_a = true;
        let mut passes = 0;
        for i in 0..profile.glow_radius {
            let weight = profile.glow_ring_weight(i);
            if weight < profile.glow_pass_weight_threshold {
                continue;
            }
            self.ring_mat.set_texture("u_main", src.clone());
            self.ring_mat.set_uniform("u_weight", weight);
            self.ring_mat.set_uniform("u_first", if passes == 0 { 1. } else { 0. });
            let dst: &RenderTarget = if use_a { &self.targets.ping_a } else { &self.targets.ping_b };
            self.pass(&self.ring_mat, dst);
            src = dst.texture.clone();
            use_a = !use_a;
            passes += 1;
        }
        self.effect_mat.set_texture("u_glow", src);
        self.pass(&self.effect_mat, &self.targets.ring);

        // ---- 5. ActiveBlock / DisabledBlock / ReadyBlock ----
        self.set_apply_uniforms(profile, w, h, t);
        self.apply_mat.set_texture("u_src", self.targets.scene.texture.clone());
        self.apply_mat.set_texture("u_cov", self.targets.cov.texture.clone());
        self.apply_mat.set_texture("u_ring", self.targets.ring.texture.clone());
        self.apply_mat.set_texture("u_spark", self.spark.clone());
        self.apply_mat.set_texture("u_disp", self.disp.clone());
        self.pass(&self.apply_mat, &self.targets.result);

        // ---- 6. 写回，恢复 render pass / viewport ----
        internal_gl().flush();
        {
            let gl = internal_gl();
            gl.quad_gl.render_pass(onto.as_ref().map(|rt| rt.render_pass.raw_miniquad_id()));
        }
        self.copy_mat.set_texture("u_src", self.targets.result.texture.clone());
        Self::draw_fullscreen(&self.copy_mat);
        internal_gl().flush();
        {
            let gl = internal_gl();
            gl.quad_gl.viewport(Some((vp[0], vp[1], vp[2], vp[3])));
        }
    }

    /// 跑一趟全屏 pass。
    fn pass(&self, material: &Material, dst: &RenderTarget) {
        {
            let gl = internal_gl();
            gl.quad_gl.render_pass(Some(dst.render_pass.raw_miniquad_id()));
        }
        Self::draw_fullscreen(material);
    }

    /// 在当前 render pass 上铺满一个四边形（position 直接是 NDC，uv 左下为原点）。
    fn draw_fullscreen(material: &Material) {
        gl_use_material(material);
        {
            let verts = [
                Vertex::new(-1., -1., 0., 0., 0., WHITE),
                Vertex::new(1., -1., 0., 1., 0., WHITE),
                Vertex::new(1., 1., 0., 1., 1., WHITE),
                Vertex::new(-1., 1., 0., 0., 1., WHITE),
            ];
            let idx: [u16; 6] = [0, 1, 2, 0, 2, 3];
            let gl = internal_gl();
            gl.quad_gl.texture(None);
            gl.quad_gl.draw_mode(DrawMode::Triangles);
            gl.quad_gl.geometry(&verts, &idx);
        }
        gl_use_default_material();
        internal_gl().flush();
    }

    fn set_compose_uniforms(&self, p: &BlockProfile, w: u32, h: u32, t: f64) {
        let (ux, uy) = p.displace_dir();
        let tx = (t / 20.) as f32; // Unity 的 _Time.x
        let m = &self.cov_mat;
        m.set_texture("u_disp", self.disp.clone());
        m.set_uniform("u_res", [w as f32, h as f32]);
        m.set_uniform("u_dir", [ux, uy]);
        m.set_uniform("u_ps", p.pixel_scale);
        m.set_uniform("u_tx", tx);
        m.set_uniform("u_stA", p.compose_st);
        m.set_uniform("u_spA", p.compose_speed);
        m.set_uniform("u_stD", p.compose_st);
        m.set_uniform("u_spD", p.compose_speed);
        m.set_uniform("u_strength", p.compose_strength);
        m.set_uniform("u_strengthD", p.compose_strength_d);
        m.set_uniform("u_subLo", p.sub_threshold_low);
        m.set_uniform("u_subHi", p.sub_threshold_high);
    }

    fn set_apply_uniforms(&self, p: &BlockProfile, w: u32, h: u32, t: f64) {
        let (ux, uy) = p.displace_dir();
        let tx = (t / 20.) as f32;
        let m = &self.apply_mat;
        m.set_uniform("u_res", [w as f32, h as f32]);
        m.set_uniform("u_dir", [ux, uy]);
        m.set_uniform("u_ps", p.pixel_scale);
        m.set_uniform("u_tx", tx);
        m.set_uniform("u_ty", t as f32);
        m.set_uniform("u_stA", p.active_st);
        m.set_uniform("u_spA", p.active_speed);
        m.set_uniform("u_stD", p.disabled_st);
        m.set_uniform("u_spD", p.disabled_speed);
        m.set_uniform("u_strength", p.displace_strength);
        m.set_uniform("u_fillA", p.active_fill);
        m.set_uniform("u_fillOpA", p.active_fill_op);
        m.set_uniform("u_fillStrA", p.active_fill_str);
        m.set_uniform("u_edgeA", p.active_edge);
        m.set_uniform("u_edgeOpA", p.active_edge_op);
        m.set_uniform("u_glowA", p.active_glow);
        m.set_uniform("u_glowIntA", p.active_glow_int);
        m.set_uniform("u_tintA", p.active_tint);
        m.set_uniform("u_sparkOpA", p.active_spark_op);
        m.set_uniform("u_sparkDispA", p.active_spark_disp);
        m.set_uniform("u_hueA", p.active_hue);
        m.set_uniform("u_sparkSTA", p.active_spark_st);
        m.set_uniform("u_dispBlendA", p.active_blend);
        m.set_uniform("u_fillD", p.disabled_fill);
        m.set_uniform("u_fillOpD", p.disabled_fill_op);
        m.set_uniform("u_tintD", p.disabled_tint);
        m.set_uniform("u_sparkOpD", p.disabled_spark_op);
        m.set_uniform("u_sparkDispD", p.disabled_spark_disp);
        m.set_uniform("u_sparkSTD", p.disabled_spark_st);
        m.set_uniform("u_shineCol", p.ready_shine);
        m.set_uniform("u_shineBright", p.ready_bright);
        m.set_uniform("u_shineSpeed", p.ready_speed);
    }

    /// 把一组方块按给定顺序画进当前 render pass（position 直接是 NDC）。
    ///
    /// `clear` 为 `Some(rt)` 时先铺一层 0（等效于把这张 RT 清空）。
    /// `weight` 乘进覆盖度（官方给 subtract 方块的是 0.1），`additive` 用加法混合叠加。
    fn emit_mask(&mut self, placements: &[BlockPlacement], order: &[usize], clear: Option<RenderTarget>, weight: f32, additive: bool) {
        self.verts.clear();
        self.idx.clear();
        if clear.is_some() {
            let zero = Color::new(0., 0., 0., 1.);
            for p in [vec2(-1., -1.), vec2(1., -1.), vec2(1., 1.), vec2(-1., 1.)] {
                self.verts.push(Vertex::new(p.x, p.y, 0., 0., 0., zero));
            }
            self.idx.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
            self.flush_mask(false);
            self.verts.clear();
            self.idx.clear();
        }
        for &bi in order {
            let p = &placements[bi];
            let a = p.alpha.clamp(0., 1.);
            // R = 「几个 subtract 方块叠在一起」的计数权重（不乘淡入淡出，否则带通打不满，
            //     淡入中的 subtract 块会整块画不出来）；
            // G = 淡入淡出系数。官方 BlockCompose 的 disabled pass 正是 `sub.x * sub.y`，
            //     x 是 SubtractBlockBlender 的带通结果、y 是 alpha，两个量必须分开存。
            let cnt = ((weight * 255.).round() as u8) as f32 / 255.;
            let ga = (a * 255.).round() as u8;
            let color = Color::new(cnt, ga as f32 / 255., 0., 1.);
            // 归一化 → NDC。
            // 注意：必须镜像 y。macroquad 在 Chart::render 里压了 y 翻转矩阵，
            // 其自带绘制（判定线 / note）会补偿这次翻转；本管线不吃 Model，
            // 若不在此处镜像，最终呈现时整层会相对画面上下颠倒（blockArea 上下镜像）。
            // 角点已经在屏幕空间旋转过（见 `BlockArea::corners`），这里只做线性映射。
            let base = self.verts.len() as u16;
            for c in p.corners {
                self.verts.push(Vertex::new(c.x * 2. - 1., -(c.y * 2. - 1.), 0., 0., 0., color));
            }
            self.idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        if !self.idx.is_empty() {
            self.flush_mask(additive);
        }
    }

    /// 铺 0 的那一趟必须关混合（加法混合下铺 0 等于什么都没写），方块那一趟按 `additive` 选。
    fn flush_mask(&self, additive: bool) {
        gl_use_material(if additive { &self.mask_add_mat } else { &self.mask_mat });
        let gl = internal_gl();
        gl.quad_gl.texture(None);
        gl.quad_gl.draw_mode(DrawMode::Triangles);
        gl.quad_gl.geometry(&self.verts, &self.idx);
    }
}
