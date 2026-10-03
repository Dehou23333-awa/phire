#version 100
// 把方块光栅化成覆盖度：直接把顶点色写出去（gl.rs 用一张 mask 材质跑六趟，
// 分别把 Active / Disabled / Ready 与各自的 subtract 画进六张单通道 RT）。
// 顶点色里只有目标通道非 0，alpha = 1 —— 因为块按 alpha 升序绘制，
// source-over 下「最后画的赢」正好等价于逐通道取 max（原版 canvas 的 lighten）。
precision highp float;

varying highp vec4 v_color;

void main() {
    gl_FragColor = v_color;
}
