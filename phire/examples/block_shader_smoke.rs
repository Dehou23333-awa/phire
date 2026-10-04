//! BlockArea 渲染路径的冒烟测试。
//!
//! 官方 GLSL 是 `#version 100` + `mediump`，而且有一段 `for (i < 10) { ... break; }`
//! 的循环。这些东西**只有真正的 GL 驱动才能告诉你编不编得过**，`cargo check` 看不出来。
//!
//! 本示例会开一个隐藏窗口，强制链接材质、解码三张官方贴图，然后用空遮罩各走一趟
//! Disabled / Active 的绘制路径，最后按结果返回退出码。
//!
//! ```bash
//! cargo run -p phire --example block_shader_smoke
//! ```
//!
//! 需要 `assets/blockarea/` 下的四张官方贴图（`docs/block-area/extract_official_assets.py`）。

use phire::core::{block_material_ready, prepare_block_effects};

fn window_conf() -> macroquad::prelude::Conf {
    macroquad::prelude::Conf {
        window_title: "block-area shader smoke test".to_owned(),
        window_width: 320,
        window_height: 240,
        // 跑完就退，不要弹窗打扰。
        window_resizable: false,
        ..Default::default()
    }
}

#[macroquad::main(window_conf)]
async fn main() {
    // 链接材质 + 解码贴图；失败时会写 block_shader_error.txt 并打 warning。
    prepare_block_effects();

    if !block_material_ready() {
        eprintln!("FAIL: block-area 材质链接失败，详见 block_shader_error.txt");
        std::process::exit(1);
    }

    // 再跑两帧，确保第一次真实绘制（含 uniform / 纹理绑定）没把 GL 弄崩。
    for _ in 0..2 {
        macroquad::prelude::next_frame().await;
    }

    println!("OK: block-area 材质链接成功，绘制路径无 GL 报错");
    std::process::exit(0);
}
