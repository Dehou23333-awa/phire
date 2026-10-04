#version 100
/* ≈ Unlit/BlockCompose（官方 pass 0 = enabled、pass 1 = disabled）
   位移后的 mask 做 |subtract - normal| 得到每像素覆盖。输出 R=Active / G=Disabled / B=Ready。

   逐行对齐 phigros.apk 里反编译出的 GLES3 片段程序：

       dir  = normalize(_DisplaceDirection.xy)
       perp = (-dir.y, dir.x)
       d1   = tex(_DisplaceMap, uv*ST + dir *(_Time.x*_DisplaceSpeed)).x - 0.5
       d2   = tex(_DisplaceMap, uv*ST + perp*(_Time.x*_DisplaceSpeed)).x - 0.5
       uv  += (dir*d1 + perp*d2) * _DisplaceStrength
       out  = abs(tex(_SubtractBlockRT, uv).x - tex(_NormalBlockRT, uv).x)

   两处和 mask 语义直接相关：
   1) 位移是**二维向量**（两次采样分别当 dir / perp 方向的分量），不是把两次采样取平均
      之后各向同性地推开；
   2) subtract 图不是原始覆盖度，而是经过 Unlit/SubtractBlockBlender 的带通：
      每个 subtract 方块贡献 0.1 并**可叠加**，只有落在 [_ClampThresholdLow,
      _ClampThresholdHigh) 里（= 恰好被一个方块盖住）才算 1，0 个和 >=2 个都是 0。
      于是互相重叠的 subtract 方块会抵消，这正是「挖洞 / 归并」的来源。 */
precision highp float;

uniform sampler2D u_maskA;
uniform sampler2D u_maskD;
uniform sampler2D u_maskR;
uniform sampler2D u_subA;
uniform sampler2D u_subD;
uniform sampler2D u_subR;

uniform sampler2D u_disp;
uniform vec2 u_res;
uniform vec2 u_dir;
uniform float u_ps;
uniform float u_tx;
uniform vec2 u_stA;
uniform float u_spA;
uniform vec2 u_stD;
uniform float u_spD;
uniform float u_strength;
uniform float u_strengthD;
uniform float u_subLo;
uniform float u_subHi;

varying vec2 v_uv;

/* 格子尺寸 = _BackgroundPixelScale 个屏幕像素：按 ps 像素切格、取格心 */
vec2 disp_snap(vec2 uv) {
    float ps = max(u_ps, 1.0);
    return (floor(uv * u_res / ps) * ps + ps * 0.5) / u_res;
}

/* 沿 dir 与其垂直方向各滚 _Time.x * _DisplaceSpeed 采位移图，合成二维偏移量 */
vec2 disp_off(vec2 uv, vec2 st, float sp, float tx, float snap) {
    vec2 perp = vec2(-u_dir.y, u_dir.x);
    vec2 base = uv * st;
    float s = tx * sp;
    vec2 q1 = base + u_dir * s;
    vec2 q2 = base + perp * s;
    if (snap > 0.5) {
        q1 = disp_snap(q1);
        q2 = disp_snap(q2);
    }
    float d1 = texture2D(u_disp, q1).r - 0.5;
    float d2 = texture2D(u_disp, q2).r - 0.5;
    return u_dir * d1 + perp * d2;
}

/* Unlit/SubtractBlockBlender 的带通判定。
   输入是「叠了几个 subtract 方块」的计数权重（每个 0.1，与淡入淡出无关），
   所以 >=2 个会超出窗口而互相抵消。 */
float sub_band(float x) {
    return (x >= u_subLo && x < u_subHi) ? 1.0 : 0.0;
}

void main() {
    vec2 uvA = v_uv + disp_off(v_uv, u_stA, u_spA, u_tx, 0.0) * u_strength;
    vec2 uvD = v_uv + disp_off(v_uv, u_stD, u_spD, u_tx, 0.0) * u_strengthD;

    /* subtract 图：R = 计数（0.1/个，加法混合累积），G = 淡入淡出系数。
       官方 BlockCompose 的 disabled pass 就是 `sub.x * sub.y`，两个量必须分开乘。 */
    vec3 subA = vec3(texture2D(u_subA, uvA).rg, 0.0);
    vec3 subD = vec3(texture2D(u_subD, uvD).rg, 0.0);
    vec3 subR = vec3(texture2D(u_subR, uvD).rg, 0.0);

    float a = abs(sub_band(subA.r) * subA.g - texture2D(u_maskA, uvA).g);
    float d = abs(sub_band(subD.r) * subD.g - texture2D(u_maskD, uvD).g);
    float r = abs(sub_band(subR.r) * subR.g - texture2D(u_maskR, uvD).g);

    gl_FragColor = vec4(clamp(a, 0.0, 1.0), clamp(d, 0.0, 1.0), clamp(r, 0.0, 1.0), 0.0);
}
