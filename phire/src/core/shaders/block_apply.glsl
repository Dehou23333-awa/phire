#version 100
/* ≈ Unlit/ActiveBlock（pid38）单趟全屏 pass 的字面翻译，外加 DisabledBlock(pid39)
   / ReadyBlock(pid36) 两项折叠进来。

   权威来源：phigros.apk 里 sh38 的 GLES3 片段程序（temp/_ds/block_shaders.txt:428-787），
   逐行对应关系写在下面的注释里（行号即该文件的行号）。
   材质数值来自 data.unity3d 的 ActiveBlock / DisabledBlock / ReadyBlock 三个 Material。 */
precision highp float;

uniform sampler2D u_src;   // _SceneColor
uniform sampler2D u_cov;   // R=_ComposeRT.x, G=disabled |sub-normal|, B=ready
uniform sampler2D u_ring;  // R=_EffectRT.x(描边), G=_EffectRT.y(辉光)
uniform sampler2D u_spark; // _SparkMap  (PointNoise, ST=(3,1.2), Repeat, Point)
uniform sampler2D u_disp;  // _DisplaceMap(BlockNoise1, ST=(0.8,0.3), Mirror, Point)

uniform vec2 u_res;
uniform vec2 u_dir;
uniform float u_ps;
uniform vec2 u_stA;
uniform float u_spA;
uniform vec2 u_stD;
uniform float u_spD;
uniform float u_strength;
uniform float u_tx;
uniform float u_ty;

uniform vec3 u_fillA;
uniform float u_fillOpA;
uniform float u_fillStrA;
uniform vec3 u_edgeA;
uniform float u_edgeOpA;
uniform vec3 u_glowA;
uniform float u_glowIntA;
uniform vec3 u_tintA;
uniform float u_sparkOpA;
uniform float u_sparkDispA;
uniform float u_hueA;
uniform vec2 u_sparkSTA;
uniform float u_dispBlendA;

uniform vec3 u_fillD;
uniform float u_fillOpD;
uniform vec3 u_tintD;
uniform float u_sparkOpD;
uniform float u_sparkDispD;
uniform vec2 u_sparkSTD;

uniform vec3 u_shineCol;
uniform float u_shineBright;
uniform float u_shineSpeed;

varying vec2 v_uv;

/* 官方 _BackgroundPixelScale：位移图按 ps 个屏幕像素一格采样，取格心（L465-470） */
vec2 snap(vec2 uv) {
    float ps = max(u_ps, 1.0);
    return (floor(uv * u_res / ps) * ps + ps * 0.5) / u_res;
}

/* 位移图两趟采样：沿 dir 滚一格、沿 perp 滚一格（L459-479）。
   d1 给 dir 分量、d2 给 perp 分量 —— 是**二维向量**，不是两次的平均值。
   返回的 off 就是 L486 的 u_xlat1.xy；avg 是 L480-481 的 u_xlat16_35 = (d1+d2)*0.5。 */
float disp_taps(vec2 uv, vec2 st, float speed, float snap_on, out vec2 off) {
    vec2 n = u_dir;
    float s = u_tx * speed;
    vec2 base = uv * st;
    vec2 q1 = base + n * s;
    vec2 q2 = base + vec2(-n.y, n.x) * s;
    if (snap_on > 0.5) {
        q1 = snap(q1);
        q2 = snap(q2);
    }
    float d1 = texture2D(u_disp, q1).r;
    float d2 = texture2D(u_disp, q2).r;
    off = n * (d1 - 0.5) + vec2(-n.y, n.x) * (d2 - 0.5);
    return (d1 + d2) * 0.5;
}

vec3 srgb_to_linear(vec3 c) { return pow(max(c, vec3(0.0)), vec3(2.2)); }
vec3 linear_to_srgb(vec3 c) { return pow(max(c, vec3(0.0)), vec3(1.0 / 2.2)); }

vec3 rgb2hsv(vec3 c) {
    vec4 K = vec4(0.0, -1.0 / 3.0, 2.0 / 3.0, -1.0);
    vec4 p = mix(vec4(c.bg, K.wz), vec4(c.gb, K.xy), step(c.b, c.g));
    vec4 q = mix(vec4(p.xyw, c.r), vec4(c.r, p.yzx), step(p.x, c.r));
    float d = q.x - min(q.w, q.y);
    return vec3(abs(q.z + (q.w - q.y) / (6.0 * d + 1e-10)), d / (q.x + 1e-10), q.x);
}

vec3 hsv2rgb(vec3 c) {
    vec4 K = vec4(1.0, 2.0 / 3.0, 1.0 / 3.0, 3.0);
    vec3 p = abs(fract(c.xxx + K.xyz) * 6.0 - K.www);
    return c.z * mix(K.xxx, clamp(p - K.xxx, 0.0, 1.0), c.y);
}

/* 官方 L570-584：以 b（色相偏移后的背景）为底、P（火花）为顶的 overlay 混合。
   分支条件在 b 上，乘法支是 2*b*P，屏幕支是 1-2(1-b)(1-P)。 */
vec3 overlay(vec3 p, vec3 b) {
    return mix(2.0 * b * p, 1.0 - 2.0 * (1.0 - b) * (1.0 - p), step(vec3(0.5), b));
}

void main() {
    /* L430-433：ActiveBlock 只在 |u-0.5| <= 0.8889*H/W 内绘制（安全区遮罩）。
       vs_TEXCOORD6 = _ScreenParams.y*0.888888896/_ScreenParams.x */
    if (abs(v_uv.x - 0.5) > 0.888888896 * u_res.y / u_res.x) {
        gl_FragColor = texture2D(u_src, v_uv);
        return;
    }

    vec4 cov = texture2D(u_cov, v_uv);
    float C = cov.r;          // _ComposeRT.x
    float dCov = cov.g;
    float rCov = cov.b;
    vec3 ring = texture2D(u_ring, v_uv).rgb;
    float e = ring.r;         // _EffectRT.x（官方按 _EffectRT_TexelSize 取格心，等价于 Point 采样）
    float g = ring.g;         // _EffectRT.y

    /* L447-453：总权重 = 填色 + 描边 + 辉光，低于 1e-4 直接不画 */
    if (C + e + g + dCov + rCov < 1e-4) {
        gl_FragColor = texture2D(u_src, v_uv);
        return;
    }

    vec3 acc = vec3(0.0);
    float alpha = 0.0;

    /* ---- ActiveBlock 主体（L454-595）---- */
    if (C + e + g > 1e-4) {
        vec2 offA;
        float avg = disp_taps(v_uv, u_stA, u_spA, 1.0, offA);

        vec3 scene = texture2D(u_src, snap(v_uv) + offA * u_strength).rgb;   // L487-496
        float sp = texture2D(u_spark, v_uv * u_sparkSTA + offA * u_sparkDispA).r; // L493-494

        /* L536-537：P = avg * (spark * _SparkTint) * _SparkMapOpacity */
        vec3 P = avg * (sp * u_tintA) * u_sparkOpA;

        /* L538-563：背景转 HSV，色相取绝对值，再按通道加 P*_SparkHueShiftAmount */
        vec3 hsv = rgb2hsv(scene);
        hsv += P * u_hueA;
        vec3 tinted = hsv2rgb(hsv);                                          // L564-571

        vec3 ov = clamp(overlay(P, tinted), 0.0, 1.0);                       // L572-585
        vec3 base = u_fillA - avg * u_dispBlendA;                            // L586
        vec3 fill = mix(base, ov, u_fillStrA);                               // L587-588

        /* L457-458：辉光要被 (1 - 描边 - 填色覆盖) 压掉，官方没夹取下界 */
        float ew = e * u_edgeOpA;                                            // L456
        float gw = g * (1.0 - e - C) * u_glowIntA;                           // L457-458

        acc += fill * C + u_edgeA * ew + u_glowA * gw;                       // L589-591
        alpha += C * u_fillOpA + ew + gw;                                    // L594-595
    }

    /* ---- DisabledBlock（pid39 L295-322）：rgb = C*(_FillColor*_FillOpacity + 火花) ---- */
    if (dCov > 1e-4) {
        vec2 offD;
        float avgD = disp_taps(v_uv, u_stD, u_spD, 0.0, offD);
        float spD = texture2D(u_spark, v_uv * u_sparkSTD + offD * u_sparkDispD).r;
        vec3 fillD = u_fillD * u_fillOpD + spD * u_tintD * avgD * u_sparkOpD;
        acc += fillD * dCov;
        alpha += dCov;
    }

    /* ---- ReadyBlock（pid36 L341-356）：只有中性白闪烁，不参与 alpha ---- */
    if (rCov > 1e-4) {
        float shine = u_shineBright * (sin(u_ty * u_shineSpeed) * 0.5 + 1.0); // L603-607
        acc += u_shineCol * shine * rCov;
    }

    alpha = clamp(alpha, 0.0, 1.0);
    vec3 bg = srgb_to_linear(texture2D(u_src, v_uv).rgb);
    gl_FragColor = vec4(linear_to_srgb(bg * (1.0 - alpha) + acc), 1.0);
}
