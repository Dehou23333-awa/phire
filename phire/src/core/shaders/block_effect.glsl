#version 100
/* ≈ Unlit/EdgeMask pass 1，并把 GlowMask 累积好的辉光带到 G。

   EdgeMask pass 1（temp/_ds/block_shaders.txt:68-105）：
       out.x = clamp(max9(_MainTex.x) - _ComposeRT.x, 0, 1)
   官方 _MainTex 与 _ComposeRT 在这里都是 composedEnabledBlockRT，所以两路都采 u_cov。

   注意官方 `RenderEffects` 收尾用的是 GlowMask **pass 1**
   （`SV_Target0.xy = vec2(0.0, _MainTex.y)`），字面理解会把 R 清成 0、描边整个丢掉；
   实测那样遮挡层几乎不可见（底带只剩 fill*覆盖 ≈ 0.26,0.10,0.11），
   而官方截图的底带是 (0.40,0.29,0.34)，所以那趟 Blit 必然是叠加混合、R 是保留的。 */
precision highp float;

uniform sampler2D u_glow;
uniform sampler2D u_cov;
uniform vec2 u_step;

varying vec2 v_uv;

void main() {
    float c = texture2D(u_cov, v_uv).r;
    float d = c;
    d = max(d, texture2D(u_cov, v_uv + vec2(u_step.x, 0.0)).r);
    d = max(d, texture2D(u_cov, v_uv - vec2(u_step.x, 0.0)).r);
    d = max(d, texture2D(u_cov, v_uv + vec2(0.0, u_step.y)).r);
    d = max(d, texture2D(u_cov, v_uv - vec2(0.0, u_step.y)).r);
    d = max(d, texture2D(u_cov, v_uv + u_step).r);
    d = max(d, texture2D(u_cov, v_uv - u_step).r);
    d = max(d, texture2D(u_cov, v_uv + vec2(u_step.x, -u_step.y)).r);
    d = max(d, texture2D(u_cov, v_uv + vec2(-u_step.x, u_step.y)).r);

    gl_FragColor = vec4(clamp(d - c, 0.0, 1.0), texture2D(u_glow, v_uv).g, 0.0, 0.0);
}
