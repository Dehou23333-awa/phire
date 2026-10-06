use super::{BlockPhase, Resource, Vector, Zone};
use crate::core::{internal_id, rescale_fbo, rgb8_render_target};
use crate::ext::make_additive_pipeline;
use macroquad::miniquad::{BlendFactor, BlendState, BlendValue, Equation, PipelineParams, RenderingBackend, TextureWrap, UniformDesc, UniformType};
use macroquad::prelude::*;
use once_cell::sync::Lazy;
use std::cell::RefCell;

#[path = "block_mask.rs"]
mod mask;
#[path = "block_touch.rs"]
mod touch;

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
    ("uDisabledFillOpacity", 0.4),
    ("uDisabledSparkOpacity", 3.5),
    ("uDisabledSparkIntensity", 2.29),
    ("uDisabledSpeed", 0.3),
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
    ("_TouchPosShineSpeed", 43.),
    ("_TouchPosBrightness", 2.),
    ("_TouchPosDarkness", 0.65),
    ("_TouchPosLowThreshold", 0.63),
];

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

const MAX_TOUCHES: usize = 10;

#[derive(Default)]
struct FrameTextures {
    uploaded_masks: Option<u64>,
    masks: mask::Masks,
    effect: Option<Texture2D>,
    aux: Option<Texture2D>,
    touch: touch::TouchMask,
    scene: Option<(RenderTarget, u32, (u32, u32))>,
    scene_warned: bool,
    clock: Option<f32>,
    additive: Option<GlPipeline>,
    flat_active: FlatMask,
    flat_disabled: FlatMask,
}

thread_local! {
    static FRAME: RefCell<FrameTextures> = RefCell::new(FrameTextures::default());
}

/// One `block_area_simple` layer: the low resolution coverage and the texture it is
/// uploaded to. The upload only happens when the zones moved, never per frame.
#[derive(Default)]
struct FlatMask {
    coverage: mask::FlatCoverage,
    texture: Option<Texture2D>,
}

impl FlatMask {
    fn texture(&mut self, dim: (usize, usize), aspect: f32, zones: &[Zone], active: bool) -> Texture2D {
        if self.coverage.render(dim.0, dim.1, aspect, zones, active) {
            let bytes = self.coverage.rgba();
            let fits = self.texture.as_ref().is_some_and(|texture| (texture.width() as usize, texture.height() as usize) == dim);
            if fits {
                self.texture
                    .clone()
                    .expect("coverage texture 刚刚才检查过")
                    .update_from_bytes(dim.0 as u32, dim.1 as u32, bytes);
            } else {
                let texture = Texture2D::from_rgba8(dim.0 as u16, dim.1 as u16, bytes);
                texture.set_filter(FilterMode::Linear);
                self.texture = Some(texture);
            }
        }
        self.texture.clone().expect("coverage texture 刚刚才创建过")
    }
}

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

static DISPLACE_TEX: Lazy<Texture2D> =
    Lazy::new(|| official_texture(include_bytes!("../../../assets/blockarea/BlockNoise1.png"), TextureWrap::Mirror));
static SPARK_TEX: Lazy<Texture2D> = Lazy::new(|| official_texture(include_bytes!("../../../assets/blockarea/PointNoise.png"), TextureWrap::Repeat));
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
        if !disabled {
            material.set_uniform("_TouchPosCount", 0_i32);
        }
        if hover {
            material.set_uniform_array("_TouchPos", &[vec2(0., 0.); MAX_TOUCHES]);
        }
        material
    })
}

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

pub fn prepare_block_effects() {
    Lazy::force(&MATERIAL);
    Lazy::force(&DISPLACE_TEX);
    Lazy::force(&SPARK_TEX);
    Lazy::force(&NOISE_TEX);
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

pub fn reset_block_effects() {
    FRAME.with(|frame| {
        let mut frame = frame.borrow_mut();
        frame.touch.reset();
        frame.clock = None;
    });
}

pub fn block_material_ready() -> bool {
    MATERIAL.as_ref().is_some()
}

pub fn visible_zones(areas: &[super::BlockArea], time: f64, aspect: f32) -> Vec<Zone> {
    areas.iter().filter_map(|area| Zone::from_area(area, time, aspect)).collect()
}

#[inline]
pub fn block_clock(song_time: f64, offset: f32) -> f32 {
    song_time as f32 + offset
}

pub fn draw_disabled_zones(res: &mut Resource, zones: &[Zone], aspect: f32, clock: f32, onto: Option<RenderTarget>) {
    FRAME.with(|frame| frame.borrow_mut().clock = Some(clock));
    draw_layer(res, zones, aspect, clock, true, onto, None);
}

pub fn draw_zones_with_touches(res: &mut Resource, zones: &[Zone], aspect: f32, touches: &[(u64, Vector)], flip_x: bool, onto: Option<RenderTarget>) {
    let clock = FRAME.with(|frame| frame.borrow().clock).unwrap_or(0.);
    draw_layer(res, zones, aspect, clock, false, onto, Some((touches, flip_x)));
}

/// The official material's two fills. The active one covers the chart, so it is a
/// straight alpha-over; the disabled one only tints the background under the
/// notes, so the material emits it with alpha 1 and lets `(One, One)` add it.
const FILL: [f32; 3] = [0.7132075, 0.23549296, 0.23549296];
const FILL_OPACITY: f32 = 0.667;
const DISABLED_FILL: [f32; 3] = [0.497, 0.13766898, 0.13766898];
const DISABLED_STRENGTH: f32 = 0.4;

/// How much smaller the `block_area_simple` coverage is than the target. Quarter
/// resolution is what the material's own mask resolves to after its 2x upsample.
const FLAT_DIVISOR: usize = 4;

fn flat_dim(width: usize, height: usize) -> (usize, usize) {
    ((width / FLAT_DIVISOR).max(1), (height / FLAT_DIVISOR).max(1))
}

fn flat_fill(disabled: bool) -> Color {
    if disabled {
        Color::new(
            DISABLED_FILL[0] * DISABLED_STRENGTH,
            DISABLED_FILL[1] * DISABLED_STRENGTH,
            DISABLED_FILL[2] * DISABLED_STRENGTH,
            1.,
        )
    } else {
        Color::new(FILL[0], FILL[1], FILL[2], FILL_OPACITY)
    }
}

/// The classic look: no mask buffer, no material, no scene blit. The zones go
/// through `FlatCoverage` first, so overlapping zones merge instead of stacking up
/// and subtract zones cancel the fill instead of drawing one more rectangle.
fn draw_flat(zones: &[Zone], aspect: f32, disabled: bool, onto: Option<RenderTarget>) {
    let (width, height) = match &onto {
        Some(target) => (target.texture.width() as usize, target.texture.height() as usize),
        None => (screen_width() as usize, screen_height() as usize),
    };
    let texture = FRAME.with(|frame| {
        let mut frame = frame.borrow_mut();
        if disabled {
            frame.flat_disabled.texture(flat_dim(width, height), aspect, zones, false)
        } else {
            frame.flat_active.texture(flat_dim(width, height), aspect, zones, true)
        }
    });

    let saved_viewport = unsafe { get_internal_gl() }.quad_gl.get_viewport();
    push_camera_state();
    set_camera(&Camera2D {
        zoom: vec2(1., aspect),
        viewport: None,
        render_target: onto,
        ..Default::default()
    });
    if disabled {
        let pipeline = FRAME.with(|frame| *frame.borrow_mut().additive.get_or_insert_with(make_additive_pipeline));
        unsafe { get_internal_gl() }.quad_gl.pipeline(Some(pipeline));
    }
    draw_texture_ex(
        &texture,
        -1.,
        -1. / aspect,
        flat_fill(disabled),
        DrawTextureParams {
            dest_size: Some(vec2(2., 2. / aspect)),
            ..Default::default()
        },
    );
    if disabled {
        unsafe { get_internal_gl() }.quad_gl.pipeline(None);
    }
    pop_camera_state();
    unsafe { get_internal_gl() }.quad_gl.viewport(Some(saved_viewport));
}

fn draw_layer(
    res: &mut Resource,
    zones: &[Zone],
    aspect: f32,
    time: f32,
    disabled: bool,
    onto: Option<RenderTarget>,
    touch_source: Option<(&[(u64, Vector)], bool)>,
) {
    let needs_hover = touch_source.is_some_and(|(list, _)| !list.is_empty());
    let needed = if disabled {
        zones.iter().any(|z| !z.active && z.opacity > 0.)
    } else {
        zones.iter().any(|z| z.active || z.ready) || needs_hover || FRAME.with(|frame| frame.borrow().touch.visible())
    };
    if !needed {
        return;
    }

    // The simple path and the missing-material fallback both skip the mask buffer,
    // the scene blit and the material entirely.
    if res.config.block_area_simple || res.config.flat_block_area() || MATERIAL.as_ref().is_none() {
        draw_flat(zones, aspect, disabled, onto);
        return;
    }

    let mut gl = unsafe { get_internal_gl() };

    let saved_viewport = gl.quad_gl.get_viewport();

    let draw_onto = onto;
    let (width, height) = match &draw_onto {
        Some(target) => (target.texture.width().max(1.) as usize, target.texture.height().max(1.) as usize),
        None => (screen_width().max(1.) as usize, screen_height().max(1.) as usize),
    };

    gl.flush();

    let mut scene = None;
    if !disabled {
        let size = (width as u32, height as u32);
        let small = ((width / 3).max(1) as u32, (height / 3).max(1) as u32);
        FRAME.with(|frame| {
            let mut frame = frame.borrow_mut();
            if frame
                .scene
                .as_ref()
                .is_none_or(|(_, _, dim)| *dim != small)
            {
                let target = rgb8_render_target(small.0, small.1);
                target.texture.set_filter(FilterMode::Nearest);
                let fbo = internal_id(target.clone());
                frame.scene = Some((target, fbo, small));
            }
            let (target, fbo, _) = frame.scene.as_ref().unwrap();
            scene = Some(target.texture.clone());
            let source = draw_onto.as_ref().map(|it| internal_id(it.clone())).unwrap_or(0);
            if !rescale_fbo(source, *fbo, size, small, true) && !frame.scene_warned {
                frame.scene_warned = true;
                eprintln!("block-area: scene downsample blit failed (src={source}, {}x{} -> {}x{})", size.0, size.1, small.0, small.1);
            }
        });
    }
    let scene = scene.unwrap_or_else(|| EMPTY_TEX.clone());

    push_camera_state();
    set_camera(&Camera2D {
        zoom: vec2(1., aspect),
        viewport: None,
        render_target: draw_onto.clone(),
        ..Default::default()
    });

    let materials = MATERIAL.as_ref().unwrap();

    let touches: Vec<(u64, Vec2)> = match touch_source {
        None => Vec::new(),
        Some((list, flip_x)) => {
            let projection = gl.quad_gl.get_projection_matrix();
            let mut touches: Vec<_> = list
                .iter()
                .map(|&(id, p)| {
                    let p = projection * vec4(if flip_x { -p.x } else { p.x }, -p.y, 0., 1.);
                    (id, vec2(p.x / p.w, p.y / p.w) * 0.5 + vec2(0.5, 0.5))
                })
                .collect();
            touches.sort_by_key(|(id, _)| *id);
            touches
        }
    };

    FRAME.with(|frame| {
        let mut frame = frame.borrow_mut();
        frame.masks.render_displaced(width, height, aspect, zones, time);
        if !disabled {
            frame.touch.update_fingers(&touches, time);
        }
        let hover = !disabled && frame.touch.visible();

        let effect_dim = (frame.masks.width as u32, frame.masks.height as u32);
        let resized = frame
            .effect
            .as_ref()
            .is_none_or(|texture| (texture.width() as u32, texture.height() as u32) != effect_dim);
        if resized {
            let texture = Texture2D::from_rgba8(effect_dim.0 as u16, effect_dim.1 as u16, &frame.masks.rgba);
            texture.set_filter(FilterMode::Linear);
            frame.effect = Some(texture);
        } else if frame.uploaded_masks != Some(frame.masks.revision) {
            let effect = frame.effect.as_ref().expect("effect texture 刚刚才创建过");
            effect.update_from_bytes(effect_dim.0, effect_dim.1, &frame.masks.rgba);
        }

        if hover {
            let bw = frame.masks.width / 2;
            let bh = frame.masks.height / 2;
            for y in 0..bh {
                for x in 0..bw {
                    let value = frame.touch.sample(vec2((x as f32 + 0.5) / bw as f32, (y as f32 + 0.5) / bh as f32), aspect);
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
        m.set_texture("uScene", scene.clone());
        m.set_texture("uAuxMasks", frame.aux.clone().expect("aux texture"));
        m.set_texture("uNoiseTex", NOISE_TEX.clone());
        m.set_uniform("uUnityTime", vec4(time / 20., time, time * 2., time * 3.));
        m.set_uniform("uView", vec3(width as f32, height as f32, aspect));
        m.set_uniform("_ScreenParams", vec4(width as f32, height as f32, 1. + 1. / width as f32, 1. + 1. / height as f32));
        m.set_uniform(
            "_EffectRT_TexelSize",
            vec4(1. / effect_dim.0 as f32, 1. / effect_dim.1 as f32, effect_dim.0 as f32, effect_dim.1 as f32),
        );
        if !disabled {
            m.set_uniform("_TouchPosCount", touches.len().min(MAX_TOUCHES) as i32);
        }
        if hover {
            let mut positions = [vec2(0., 0.); MAX_TOUCHES];
            for (slot, (_, uv)) in positions.iter_mut().zip(touches.iter()) {
                *slot = *uv * vec2(width as f32 / height as f32, 1.);
            }
            m.set_uniform_array("_TouchPos", &positions);
        }
        m.set_uniform("_TouchPosShine", (0.63 + 0.37 * ((time * 43.).sin() * 0.5 + 0.5)) * 2.);

        gl_use_material(m);
        draw_rectangle(-1., -1. / aspect, 2., 2. / aspect, WHITE);
        gl_use_default_material();
    });

    pop_camera_state();
    unsafe { get_internal_gl() }.quad_gl.viewport(Some(saved_viewport));
}

const VERTEX: &str = include_str!("shaders/block_full_vert.glsl");
const FRAGMENT: &str = include_str!("shaders/block_full.glsl");

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn shader_specialization_anchors_exist() {
        assert!(FRAGMENT.contains("uniform int uLayer;"), "uLayer 特化锚点丢了");
        assert!(FRAGMENT.contains("uniform \tint _TouchPosCount;"), "_TouchPosCount 特化锚点丢了");
        assert!(FRAGMENT.contains("float hoverSample(vec2 uv) { return texture2D(uAuxMasks, basePixelUV(uv)).a; }"), "hoverSample 特化锚点丢了");
    }

    #[test]
    fn vertex_uniforms_are_registered() {
        for name in ["uView"] {
            assert!(FRAGMENT.contains(name) || VERTEX.contains(name));
        }
        assert!(VERTEX.contains("uniform vec3 uView;"));
        assert!(FLOATS.iter().any(|(name, _)| *name == "_EdgeOpacity"));
    }

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

        assert!(Zone::from_area(&area, 0.5, 2.).is_none());
        let z = Zone::from_area(&area, 1.25, 2.).unwrap();
        assert!(!z.active && !z.ready && (z.opacity - 0.5).abs() < 1e-6, "{z:?}");
        let z = Zone::from_area(&area, 2.75, 2.).unwrap();
        assert!(z.ready && !z.active, "{z:?}");
        let z = Zone::from_area(&area, 4., 2.).unwrap();
        assert!(z.active && !z.ready && z.opacity == 1., "{z:?}");
        area.top_right = area.bottom_left;
        assert!(Zone::from_area(&area, 4., 2.).is_none());
        area.top_right = Vector::new(0.6, 0.6);
        area.is_subtract = true;
        assert!(Zone::from_area(&area, 4., 2.).unwrap().invert);
    }
}
