#version 100
/* ≈ Unlit/GlowMask pass 0 —— 一趟 1 像素的 3×3 膨胀 + 加权累加。
   官方在 BlockRender.RenderEffects 里用 pingA/pingB 来回跑 glowRadius 趟，
   每趟的权重是 GetGlowRingWeight(i, glowRadius, glowWeightFalloff)。

   逐行对齐 temp/_ds/block_shaders.txt:118-161：
       d     = max9(_MainTex.x)
       ring  = clamp(d - self.x, 0, 1) * (1 - _ComposeRT.x)   // 辉光不进方块内部
       out.x = d                                              // 把「已膨胀的轮廓」传给下一趟
       out.y = (首趟 ? 0 : self.y) + ring * _PassWeight        */
precision highp float;

uniform sampler2D u_main;
uniform sampler2D u_cov;
uniform vec2 u_step;
uniform float u_weight;
uniform float u_first;

varying vec2 v_uv;

void main() {
    vec2 m = texture2D(u_main, v_uv).rg;
    float d = m.r;
    d = max(d, texture2D(u_main, v_uv + vec2(u_step.x, 0.0)).r);
    d = max(d, texture2D(u_main, v_uv - vec2(u_step.x, 0.0)).r);
    d = max(d, texture2D(u_main, v_uv + vec2(0.0, u_step.y)).r);
    d = max(d, texture2D(u_main, v_uv - vec2(0.0, u_step.y)).r);
    d = max(d, texture2D(u_main, v_uv + u_step).r);
    d = max(d, texture2D(u_main, v_uv - u_step).r);
    d = max(d, texture2D(u_main, v_uv + vec2(u_step.x, -u_step.y)).r);
    d = max(d, texture2D(u_main, v_uv + vec2(-u_step.x, u_step.y)).r);

    float ring = clamp(d - m.r, 0.0, 1.0) * (1.0 - texture2D(u_cov, v_uv).r);
    float accum = mix(m.g, 0.0, u_first) + ring * u_weight;
    gl_FragColor = vec4(d, clamp(accum, 0.0, 1.0), 0.0, 0.0);
}
