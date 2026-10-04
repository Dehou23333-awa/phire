//! BlockArea（官方叫「噪域」）的渲染。
//!
//! 结构对应官方管线，与 Phira Pro 的实现同构：
//!
//! 1. `block_mask.rs` 在 1/8 分辨率上把区域栅格化成六个相机源，再做 Compose 位移、
//!    EdgeMask 膨胀、GlowMask 累积，产出两张 RGBA 遮罩贴图；
//! 2. 这里把它们连同三张官方贴图（`BlockNoise1` / `PointNoise` / `FD_Noise_00000`）
//!    一起喂给一条**全屏**材质，材质里就是导出出来的原生 GLSL
//!    （`shaders/block_full.glsl`，逐字符保留官方运算顺序与精度标注）。
//!
//! 分成两趟，顺序与官方一致：
//!
//! * **Disabled / Ready**：Background sorting layer order 2，画在判定线**之前**，
//!   由 `Chart::render` 内部调用 [`draw_disabled_zones`]；
//! * **Active**：官方跑在 `CameraEvent.AfterForwardAlpha`，也就是音符 + HUD **之后**，
//!   采样完整底图，由 `GameScene` 在 UI 之后调用 [`draw_zones_with_touches`]。
//!
//! 材质常量直接来自官方 `.mat`，可用 `docs/block-area/audit_material_constants.py`
//! 对 APK 重新核对（任何漂移都会立刻报出来）。

use super::{Matrix, Resource, Vector, Zone};
use macroquad::material::{gl_use_default_material, gl_use_material, load_material, Material, MaterialParams};
use macroquad::miniquad::{BlendFactor, BlendState, BlendValue, Equation, PipelineParams, RenderingBackend, TextureWrap, UniformDesc, UniformType};
use macroquad::prelude::*;
use once_cell::sync::Lazy;
use std::cell::RefCell;

#[path = "block_mask.rs"]
mod mask;
#[path = "block_touch.rs"]
mod touch;

/// 官方 `ActiveBlock.mat` / `DisabledBlock.mat` 的 float 参数。
///
/// 顺序无关紧要；数值必须与 APK 一致，改之前先跑
/// `python docs/block-area/audit_material_constants.py <apk>`。
const FLOATS: &[(&str, f32)] = &[
    ("_EdgeOpacity", 0.8),
    ("_FillStrength", 0.667),
    ("_FillOpacity", 0.667),
    ("_GlowIntensity", 0.8),
    ("_SparkMapOpacity", 5.69),
    ("_SparkHueShiftAmount", 0.2),
    ("_SparkDisplaceIntensity", 2.39),
    ("_DisplaceBlendIntensity", 0.411),
    ("_DisplaceSpeed", 1.5),
    ("_DisplaceStrength", 0.15),
    ("_TouchPosShine", 0.),
    ("_TouchPosRadius", 0.5),
    ("_TouchPosSDFSmoothness", 0.47),
    ("_TouchPosSDFFalloff", 0.41),
    ("_BackgroundPixelScale", 6.),
    // DisabledBlock.mat 里的对应字段（Phira Pro 的命名）。
    ("uDisabledFillOpacity", 0.4),
    ("uDisabledSparkOpacity", 3.5),
    ("uDisabledSparkIntensity", 2.29),
    ("uDisabledSpeed", 0.3),
    // ActiveBlock。
    ("_ShineSpeed", 37.9),
    ("_ShineBrightness", 0.12),
    ("_TouchDisplaceSpeed", 2.9),
    ("_TouchDisplaceStrength", 0.08),
    ("_NoiseEvoSpeed", 0.03),
    ("_NoiseDirChangeSpeed", 60.),
    ("_NoiseDisplaceStrength", 1.),
    ("_NoiseRadius", 0.48),
    ("_NoiseSmoothness", 1.),
    ("_SDFCellSize", 0.11),
    ("_SDFSmoothness", 0.63),
    ("_SDFFalloff", 0.34),
    ("_SDFMoveSpeed", 9.3),
    ("_TouchBackgroundPixelScale", 8.),
    // 参考 Shine / TouchPos 用（官方由 `Shader.SetGlobal` 写入，材质里查不到）。
    ("_TouchPosShineSpeed", 43.),
    ("_TouchPosBrightness", 2.),
    ("_TouchPosDarkness", 0.65),
    ("_TouchPosLowThreshold", 0.63),
];

/// 颜色参数。
const COLORS: &[(&str, [f32; 4])] = &[
    ("_EdgeColor", [1., 0.33018857, 0.33018857, 1.]),
    ("_FillColor", [0.7132075, 0.23549296, 0.23549296, 1.]),
    ("_GlowColor", [1., 0.17924517, 0.17924517, 1.]),
    ("_DisplaceDirection", [1., 1., 0., 0.]),
    ("uDisabledFillColor", [0.497, 0.13766898, 0.13766898, 1.]),
    ("_ShineColor", [1., 1., 1., 1.]),
    ("_TouchDisplaceDirection", [1., 1., 0., 0.]),
    ("_NoiseTint", [1., 0., 0., 1.]),
    ("_TouchGlowColor", [1., 0., 0., 1.]),
];

/// 最多同时参与 SDF 的触点数（官方 `_TouchPos[10]`）。
const MAX_TOUCHES: usize = 10;

#[derive(Default)]
struct FrameTextures {
    /// 上一帧上传的遮罩版本号，避免重复上传。
    uploaded_masks: Option<u64>,
    masks: mask::Masks,
    effect: Option<Texture2D>,
    aux: Option<Texture2D>,
    touch: touch::TouchMask,
    /// Unity `_Time` 在同一帧的所有 pass 里是同一个值，这里缓存它。
    clock: Option<f32>,
}

thread_local! {
    static FRAME: RefCell<FrameTextures> = RefCell::new(FrameTextures::default());
}

/// 按官方 `m_TextureSettings` 上传一张贴图。
///
/// 导出的 PNG 行序是自上而下，而 Unity GLES 的纹理坐标自下而上，所以上传前翻转。
fn official_texture(bytes: &[u8], wrap: TextureWrap) -> Texture2D {
    let source = image::load_from_memory(bytes).expect("官方 block-area 贴图解码失败").to_rgba8();
    let source = image::imageops::flip_vertical(&source);
    let texture = Texture2D::from_rgba8(source.width() as u16, source.height() as u16, source.as_raw());
    texture.set_filter(FilterMode::Nearest);
    unsafe { get_internal_gl() }
        .quad_context
        .texture_set_wrap(texture.raw_miniquad_id(), wrap, wrap);
    texture
}

/// 官方 `BlockNoise1`（位移图，wrap=Mirror）。
static DISPLACE_TEX: Lazy<Texture2D> =
    Lazy::new(|| official_texture(include_bytes!("../../../assets/blockarea/BlockNoise1.png"), TextureWrap::Mirror));
/// 官方 `PointNoise`（火花图，wrap=Repeat）。
static SPARK_TEX: Lazy<Texture2D> = Lazy::new(|| official_texture(include_bytes!("../../../assets/blockarea/PointNoise.png"), TextureWrap::Repeat));
/// 官方 `FD_Noise_00000`（SDF 噪声，wrap=Mirror）。
static NOISE_TEX: Lazy<Texture2D> =
    Lazy::new(|| official_texture(include_bytes!("../../../assets/blockarea/FD_Noise_00000.png"), TextureWrap::Mirror));
static EMPTY_TEX: Lazy<Texture2D> = Lazy::new(|| Texture2D::from_rgba8(1, 1, &[0; 4]));

fn load_block_material(disabled: bool, hover: bool) -> Result<Material, macroquad::Error> {
    let mut uniforms = vec![
        ("uView".to_owned(), UniformType::Float3),
        ("uUnityTime".to_owned(), UniformType::Float4),
        ("_ScreenParams".to_owned(), UniformType::Float4),
        ("_EffectRT_TexelSize".to_owned(), UniformType::Float4),
        ("_SparkTint".to_owned(), UniformType::Float3),
        ("uDisabledSparkTint".to_owned(), UniformType::Float3),
        ("_TouchPosCount".to_owned(), UniformType::Int1),
        ("uLayer".to_owned(), UniformType::Int1),
    ];
    uniforms.extend(FLOATS.iter().map(|(name, _)| (name.to_string(), UniformType::Float1)));
    uniforms.extend(COLORS.iter().map(|(name, _)| (name.to_string(), UniformType::Float4)));
    uniforms.push(("_TouchPos".to_owned(), UniformType::Float2));

    let params = MaterialParams {
        // ActiveBlock 是 One/OneMinusSrcAlpha；DisabledBlock 是 One/One。
        pipeline_params: PipelineParams {
            color_blend: Some(BlendState::new(
                Equation::Add,
                BlendFactor::One,
                if disabled {
                    BlendFactor::One
                } else {
                    BlendFactor::OneMinusValue(BlendValue::SourceAlpha)
                },
            )),
            ..Default::default()
        },
        uniforms: uniforms
            .into_iter()
            .map(|(name, uniform_type)| {
                let array_count = if name == "_TouchPos" { MAX_TOUCHES } else { 1 };
                UniformDesc {
                    name,
                    uniform_type,
                    array_count,
                }
            })
            .collect(),
        textures: vec![
            "uDisplaceTex".to_owned(),
            "uSparkTex".to_owned(),
            "uMasks".to_owned(),
            "uScene".to_owned(),
            "uAuxMasks".to_owned(),
            "uNoiseTex".to_owned(),
        ],
    };

    // 把只影响分支的 uniform 直接特化掉：移动 GPU 上无 hover 时，
    // 那段 SDF 会白白吃掉寄存器和指令。
    let mut fragment = FRAGMENT.replace("uniform int uLayer;", if disabled { "const int uLayer = 0;" } else { "const int uLayer = 3;" });
    if !hover {
        fragment = fragment
            .replace("uniform \tint _TouchPosCount;", "const int _TouchPosCount = 0;")
            .replace("float hoverSample(vec2 uv) { return texture2D(uAuxMasks, basePixelUV(uv)).a; }", "float hoverSample(vec2 uv) { return 0.0; }");
    }

    load_material(
        ShaderSource::Glsl {
            vertex: VERTEX,
            fragment: &fragment,
        },
        params,
    )
    .map(|material| {
        for (name, value) in FLOATS {
            material.set_uniform(name, *value);
        }
        for (name, value) in COLORS {
            material.set_uniform(name, *value);
        }
        material.set_uniform("_SparkTint", vec3(1., 0.28490567, 0.28490567));
        material.set_uniform("uDisabledSparkTint", vec3(0.31132078, 0.077830195, 0.077830195));
        // `uLayer` 与 `_TouchPosCount` 已经在特化时被替换成常量，设了只会拿到
        // “non-existing uniform” 警告；`_TouchPos` 数组在没有 hover 分支时也会被
        // 编译器整段删掉，所以只在真正用得到的那份材质上设。
        if !disabled {
            material.set_uniform("_TouchPosCount", 0_i32);
        }
        if hover {
            // 数组要**整段**上传：miniquad 把 `_TouchPos` 当作一个 array uniform
            // （`glUniform2fv(loc, array_count, data)`），单独设 `_TouchPos[i]` 会找不到。
            material.set_uniform_array("_TouchPos", &[vec2(0., 0.); MAX_TOUCHES]);
        }
        material
    })
}

/// `[Disabled, Active(无 hover), Active(有 hover)]` —— 编译期裁掉用不到的分支。
static MATERIAL: Lazy<Option<[Material; 3]>> = Lazy::new(|| {
    (|| -> Result<[Material; 3], macroquad::Error> {
        Ok([
            load_block_material(true, false)?,
            load_block_material(false, false)?,
            load_block_material(false, true)?,
        ])
    })()
    .map_err(|e| {
        tracing::warn!("block-area shader failed: {e}");
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::fs::write(exe.with_file_name("block_shader_error.txt"), format!("{e}"));
        }
    })
    .ok()
});

/// 在加载谱面时链接 shader、解码贴图，避免第一片噪域出现时卡顿一帧。
pub fn prepare_block_effects() {
    Lazy::force(&MATERIAL);
    Lazy::force(&DISPLACE_TEX);
    Lazy::force(&SPARK_TEX);
    Lazy::force(&NOISE_TEX);
    // 有些 GLES 驱动直到第一次绘制才真正编译 shader：这里用空遮罩（fragment 全 discard）
    // 走一趟，不切换 framebuffer / camera，也不会在加载画面上留下痕迹。
    if let Some(materials) = MATERIAL.as_ref() {
        let empty = EMPTY_TEX.clone();
        for material in materials {
            material.set_texture("uDisplaceTex", DISPLACE_TEX.clone());
            material.set_texture("uSparkTex", SPARK_TEX.clone());
            material.set_texture("uNoiseTex", NOISE_TEX.clone());
            for sampler in ["uMasks", "uAuxMasks", "uScene"] {
                material.set_texture(sampler, empty.clone());
            }
            material.set_uniform("uView", vec3(1., 1., 1.));
            material.set_uniform("uUnityTime", vec4(0., 0., 0., 0.));
            material.set_uniform("_ScreenParams", vec4(1., 1., 2., 2.));
            material.set_uniform("_EffectRT_TexelSize", vec4(1., 1., 1., 1.));
            gl_use_material(material);
            draw_rectangle(0., 0., 0.001, 0.001, WHITE);
        }
        gl_use_default_material();
        unsafe { get_internal_gl() }.flush();
    }
}

/// 重试 / 换谱时清掉 hover 与帧内时钟。
pub fn reset_block_effects() {
    FRAME.with(|frame| {
        let mut frame = frame.borrow_mut();
        frame.touch.reset();
        frame.clock = None;
    });
}

/// 材质是否链接成功。
///
/// 需要先调用 [`prepare_block_effects`]（或跑过一帧）。
/// 主要给 `examples/block_shader_smoke.rs` 之类的诊断用。
pub fn block_material_ready() -> bool {
    MATERIAL.as_ref().is_some()
}

/// 可见区域列表（`Hidden` 段与零尺寸的会被剔掉）。
pub fn visible_zones(areas: &[super::BlockArea], time: f64, aspect: f32) -> Vec<Zone> {
    areas.iter().filter_map(|area| Zone::from_area(area, time, aspect)).collect()
}

/// 画 Disabled / Ready 层。必须由 `Chart::render` 在判定线**之前**调用
/// （官方 sorting layer order 2）。
pub fn draw_disabled_zones(res: &mut Resource, aspect: f32, zones: &[Zone]) {
    // Unity 的 `_Time` 在一帧里所有 pass 共用一个值；即使这一层是空的也要记下时钟。
    let time = get_time() as f32;
    FRAME.with(|frame| frame.borrow_mut().clock = Some(time));
    draw_layer(res, aspect, zones, time, true, &[]);
}

/// 画 Active 层（含触点 hover）。必须由 `GameScene` 在 HUD / UI **之后**调用
/// （官方 `CameraEvent.AfterForwardAlpha`）。
///
/// `touches` 里的位置是 chart 空间；ID 用来保持原生槽位生命周期。
pub fn draw_zones_with_touches(res: &mut Resource, aspect: f32, zones: &[Zone], touches: &[(u64, Vector)], flip_x: bool) {
    // 官方 `_TouchPos` 是相机归一化屏幕坐标，所以要走一遍当前投影矩阵。
    let projection = unsafe { get_internal_gl() }.quad_gl.get_projection_matrix();
    let mut touches: Vec<_> = touches
        .iter()
        .map(|&(id, p)| {
            let p = projection * vec4(if flip_x { -p.x } else { p.x }, -p.y, 0., 1.);
            (id, vec2(p.x / p.w, p.y / p.w) * 0.5 + vec2(0.5, 0.5))
        })
        .collect();
    // 判定那边的触点来自 HashMap，加一根手指/松一根手指时顺序会变，
    // 这里按 ID 排序让 SDF 累加顺序稳定。
    touches.sort_by_key(|(id, _)| *id);
    let time = FRAME.with(|frame| frame.borrow().clock).unwrap_or_else(|| get_time() as f32);
    draw_layer(res, aspect, zones, time, false, &touches);
}

fn draw_layer(res: &mut Resource, aspect: f32, zones: &[Zone], time: f32, disabled: bool, touches: &[(u64, Vec2)]) {
    let needed = if disabled {
        zones.iter().any(|z| !z.active && z.opacity > 0.)
    } else {
        zones.iter().any(|z| z.active || z.ready) || !touches.is_empty() || FRAME.with(|frame| frame.borrow().touch.visible())
    };
    if !needed {
        return;
    }

    let Some(materials) = MATERIAL.as_ref() else {
        // shader 编译失败时的兜底：至少让人看出区域在哪。
        for z in zones.iter().filter(|z| !z.invert && z.active != disabled) {
            let m = Matrix::new_translation(&z.center)
                * nalgebra::Rotation2::new(z.angle).to_homogeneous()
                * Matrix::identity().append_nonuniform_scaling(&(z.half * 2.));
            res.apply_model_of(&m, |_| {
                draw_rectangle(-0.5, -0.5, 1., 1., Color::new(1., 0., 0., if z.active { 0.4 } else { 0.12 }));
            });
        }
        return;
    };

    let mut gl = unsafe { get_internal_gl() };
    let viewport = gl.quad_gl.get_viewport();
    gl.flush();
    let (width, height) = (viewport.2.max(1) as usize, viewport.3.max(1) as usize);

    // Active 层采样底图。用谱面 render target 的双缓冲：交换后从旧图采样、往新图写，
    // 与 `core/effect.rs` 的后处理同一条路子，不需要额外拷一次 framebuffer。
    let mut scene = None;
    if !disabled {
        if let Some(target) = res.chart_target.as_mut() {
            target.swap();
            scene = Some(target.old().texture);
            gl.quad_gl.render_pass(Some(target.output().render_pass.raw_miniquad_id()));
        }
    }
    let scene = scene.unwrap_or_else(|| EMPTY_TEX.clone());

    FRAME.with(|frame| {
        let mut frame = frame.borrow_mut();
        frame.masks.render_displaced(width, height, aspect, zones, time);
        if !disabled {
            frame.touch.update_fingers(touches, time);
        }
        let hover = !disabled && frame.touch.visible();

        // ---- 遮罩上传（尺寸变化才重新分配）----
        let effect_dim = (frame.masks.width as u32, frame.masks.height as u32);
        let resized = frame
            .effect
            .as_ref()
            .is_none_or(|texture| (texture.width() as u32, texture.height() as u32) != effect_dim);
        if resized {
            let texture = Texture2D::from_rgba8(effect_dim.0 as u16, effect_dim.1 as u16, &frame.masks.rgba);
            texture.set_filter(FilterMode::Linear);
            // 直接替换；macroquad 用 Arc 管理纹理，旧的那张会被释放。
            frame.effect = Some(texture);
        } else if frame.uploaded_masks != Some(frame.masks.revision) {
            let effect = frame.effect.as_ref().expect("effect texture 刚刚才创建过");
            effect.update_from_bytes(effect_dim.0, effect_dim.1, &frame.masks.rgba);
        }

        // hover 由 CPU 写进 aux 的 alpha：1/8 分辨率逐点采样后复制成 2×2。
        if hover {
            let bw = frame.masks.width / 2;
            let bh = frame.masks.height / 2;
            for y in 0..bh {
                for x in 0..bw {
                    let value = frame
                        .touch
                        .sample(vec2((x as f32 + 0.5) / bw as f32, (y as f32 + 0.5) / bh as f32), width as f32 / height as f32);
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let i = ((y * 2 + dy) * frame.masks.width + x * 2 + dx) * 4 + 3;
                            frame.masks.aux_rgba[i] = value;
                        }
                    }
                }
            }
        }
        let aux_resized = frame
            .aux
            .as_ref()
            .is_none_or(|texture| (texture.width() as u32, texture.height() as u32) != effect_dim);
        if aux_resized {
            let texture = Texture2D::from_rgba8(effect_dim.0 as u16, effect_dim.1 as u16, &frame.masks.aux_rgba);
            texture.set_filter(FilterMode::Linear);
            frame.aux = Some(texture);
        } else if hover || frame.uploaded_masks != Some(frame.masks.revision) {
            let aux = frame.aux.as_ref().expect("aux texture 刚刚才创建过");
            aux.update_from_bytes(effect_dim.0, effect_dim.1, &frame.masks.aux_rgba);
        }
        frame.uploaded_masks = Some(frame.masks.revision);

        let m = &materials[if disabled {
            0
        } else if hover {
            2
        } else {
            1
        }];
        m.set_texture("uDisplaceTex", DISPLACE_TEX.clone());
        m.set_texture("uSparkTex", SPARK_TEX.clone());
        m.set_texture("uMasks", frame.effect.clone().expect("effect texture"));
        m.set_texture("uScene", scene);
        m.set_texture("uAuxMasks", frame.aux.clone().expect("aux texture"));
        m.set_texture("uNoiseTex", NOISE_TEX.clone());
        m.set_uniform("uUnityTime", vec4(time / 20., time, time * 2., time * 3.));
        m.set_uniform("uView", vec3(width as f32, height as f32, aspect));
        m.set_uniform("_ScreenParams", vec4(width as f32, height as f32, 1. + 1. / width as f32, 1. + 1. / height as f32));
        m.set_uniform("_EffectRT_TexelSize", vec4(1. / effect_dim.0 as f32, 1. / effect_dim.1 as f32, effect_dim.0 as f32, effect_dim.1 as f32));
        if !disabled {
            m.set_uniform("_TouchPosCount", touches.len().min(MAX_TOUCHES) as i32);
        }
        if hover {
            // 见 `load_block_material`：这是 array uniform，必须一次传满。
            let mut positions = [vec2(0., 0.); MAX_TOUCHES];
            for (slot, (_, uv)) in positions.iter_mut().zip(touches.iter()) {
                *slot = *uv * vec2(width as f32 / height as f32, 1.);
            }
            m.set_uniform_array("_TouchPos", &positions);
        }
        m.set_uniform("_TouchPosShine", (0.63 + 0.37 * ((time * 43.).sin() * 0.5 + 0.5)) * 2.);

        gl_use_material(m);
        // 全屏四边形。**翻转由调用方负责**，这里不能再加一层：
        // Disabled 层是在 `Chart::render` 的 y 翻转里调的，Active 层由
        // `Chart::render_block_overlay` 自己包一层；两边都保证恰好翻一次，
        // 这样「模型空间 position → fieldUV」与「遮罩行序」才同向。
        draw_rectangle(-1., -1. / aspect, 2., 2. / aspect, WHITE);
        gl_use_default_material();
    });
}

const VERTEX: &str = include_str!("shaders/block_full_vert.glsl");
const FRAGMENT: &str = include_str!("shaders/block_full.glsl");

#[cfg(test)]
mod tests {
    use super::*;

    /// 硬编码的材质常量必须与官方 `.mat` 的字段一一对应。
    /// 数值本身的核对需要 APK，见 `docs/block-area/audit_material_constants.py`。
    #[test]
    fn material_tables_are_well_formed() {
        assert!(FLOATS.iter().all(|(name, value)| !name.is_empty() && value.is_finite()));
        assert!(COLORS.iter().all(|(name, value)| !name.is_empty() && value.iter().all(|v| v.is_finite())));
        let mut names: Vec<_> = FLOATS.iter().map(|(name, _)| *name).chain(COLORS.iter().map(|(name, _)| *name)).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "材质参数名重复");
    }

    /// shader 里特化分支用的字符串必须原样存在，否则 `.replace` 静默失效。
    #[test]
    fn shader_specialization_anchors_exist() {
        assert!(FRAGMENT.contains("uniform int uLayer;"), "uLayer 特化锚点丢了");
        assert!(FRAGMENT.contains("uniform \tint _TouchPosCount;"), "_TouchPosCount 特化锚点丢了");
        assert!(FRAGMENT.contains("float hoverSample(vec2 uv) { return texture2D(uAuxMasks, basePixelUV(uv)).a; }"), "hoverSample 特化锚点丢了");
    }

    /// 顶点着色器里用到的 uniform 必须都在 Rust 侧注册过。
    #[test]
    fn vertex_uniforms_are_registered() {
        for name in ["uView"] {
            assert!(FRAGMENT.contains(name) || VERTEX.contains(name));
        }
        assert!(VERTEX.contains("uniform vec3 uView;"));
        assert!(FLOATS.iter().any(|(name, _)| *name == "_EdgeOpacity"));
    }

    /// `Zone::from_area` 的生命周期映射。
    #[test]
    fn zone_reflects_phase_ready_and_fade() {
        use crate::core::BlockArea;
        let mut area = BlockArea {
            top_right: Vector::new(0.6, 0.6),
            bottom_left: Vector::new(0.4, 0.4),
            appear_time: 1.,
            enable_time: 3.,
            disable_time: 5.,
            disappear_time: 6.,
            ..Default::default()
        };

        // Hidden 之外才有 Zone。
        assert!(Zone::from_area(&area, 0.5, 2.).is_none());
        // 刚出现：淡入从 0 开始涨。
        let z = Zone::from_area(&area, 1.25, 2.).unwrap();
        assert!(!z.active && !z.ready && (z.opacity - 0.5).abs() < 1e-6, "{z:?}");
        // Ready 窗口。
        let z = Zone::from_area(&area, 2.75, 2.).unwrap();
        assert!(z.ready && !z.active, "{z:?}");
        // Active。
        let z = Zone::from_area(&area, 4., 2.).unwrap();
        assert!(z.active && !z.ready && z.opacity == 1., "{z:?}");
        // 尺寸为零的块被剔掉。
        area.top_right = area.bottom_left;
        assert!(Zone::from_area(&area, 4., 2.).is_none());
        // subtract 标记透传。
        area.top_right = Vector::new(0.6, 0.6);
        area.is_subtract = true;
        assert!(Zone::from_area(&area, 4., 2.).unwrap().invert);
    }
}
