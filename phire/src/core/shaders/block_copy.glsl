#version 100
// 直通拷贝：把上一步的结果贴回当前渲染目标。
//
// 之所以不用 glBlitFramebuffer：开启抗锯齿（sampleCount > 1）时当前目标是多重采样 FBO，
// 而 GL 只允许「从多重采样 FBO 解析出去」，**不允许往它里面 blit**（GL_INVALID_OPERATION），
// 那样这一层会整个消失。作为普通三角形画进去就没有这个限制。
precision highp float;

uniform sampler2D u_src;

varying highp vec2 v_uv;

void main() {
    gl_FragColor = texture2D(u_src, v_uv);
}
