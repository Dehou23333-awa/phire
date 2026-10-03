#version 100
// 全屏 / 任意四边形通用顶点着色器。
//
// 与 index/shader/fx.vert 的差别：那边顶点直接就是 NDC（靠 a_pos 铺满），
// 这里走 macroquad 的顶点格式，position 由 CPU 端按 NDC 给好（见 gl.rs），
// 着色器**不吃 Model / Projection** —— phire 在 Chart::render 里压了一个 y 翻转的
// model 矩阵，吃了它整条管线就会上下颠倒，而这里的 pass 全屏铺满、与相机无关。
//
// uv 约定与原版一致：v = 0 在渲染目标底边（texcoord.y = 0 对应 position.y = -1）。
// color0 是 0..255 的字节属性（macroquad 默认顶点着色器同样除以 255）。
attribute vec3 position;
attribute vec2 texcoord;
attribute vec4 color0;

varying vec2 v_uv;
varying vec4 v_color;

void main() {
    gl_Position = vec4(position, 1.0);
    v_uv = texcoord;
    v_color = color0 / 255.0;
}
