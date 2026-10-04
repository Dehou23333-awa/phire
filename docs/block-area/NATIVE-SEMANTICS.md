# BlockArea 原生语义核验记录

本文件记录**直接对 `libil2cpp.so` 反汇编**得到的结论，作为 `phire/src/core/block.rs`
的唯一基准。不依赖任何二手转述（包括 Phira Pro 的注释）。

* APK：`Phigros_157.apk`（4.0.1）；反汇编用 `Phigros_155.apk`（4.0.0）的
  `libil2cpp_arm64.so` + `output155/dump.cs`（Il2CppDumper，`Offset:` 即文件偏移）。
* 两个版本的 `BlockArea` / `PreviewBlockControl` 字段布局一致。
* 工具：`capstone`（ARM64）。脚本见 `docs/block-area/native/`（gitignore）。

---

## 1. 数据结构（`GameInformation.*`，字段偏移直接来自 `dump.cs`）

```
BlockArea        0x10 topRightPercentage : Vector2
                 0x18 bottomLeftPercentage : Vector2
                 0x20 appearTime   : f32
                 0x24 enableTime   : f32
                 0x28 disableTime  : f32
                 0x2C disappearTime: f32
                 0x30 isSubtract   : bool
                 0x38 rotateEvents : List<RotateEvent>
                 0x40 moveEvents   : List<MoveEvent>
                 0x48 scaleEvents  : List<ScaleEvent>

RotateEvent      0x10 anchor: Vector2, 0x18 time: f32, 0x1C easeType: i32, 0x20 rotation: f32
MoveEvent        0x10 endPosition: Vector2, 0x18 time: f32, 0x1C easeTypeX: i32, 0x20 easeTypeY: i32
ScaleEvent       0x10 anchor: Vector2, 0x18 time: f32, 0x1C easeTypeX: i32, 0x20 easeTypeY: i32,
                 0x24 scale: Vector2

PreviewBlockControl  0x40 screenWidth, 0x44 screenHeight
                     0x50 renderer (SpriteRenderer)
                     0x58 disabledBlockShowDuration  (渐显时长)
                     0x5C disabledBlockReadyDuration (Ready 窗口时长)
                     0x68 enabledLayer, 0x70 disabledLayer, 0x78 readyLayer, 0x80 touchLayer
```

> ⚠️ 注意 `disabledBlockShowDuration` 与 `disabledBlockReadyDuration` 是**两个**独立字段。
> Ready 窗口用的是 0x5C（见 §5），渐显用的是 0x58。

---

## 2. 缓动（`GetEase`）—— 已完整还原

`GetEaseWithProgress(progress, type)`（0x1CAA190）：

```
i = (int)(progress * 100.0f);        // fcvtzs：向零截断，不是 floor
if (i >= 100) return table[100];
if (i < 0)    return table[0];
return table[i] + (progress*100 - i) * (table[i+1] - table[i]);
```

`GetEase.Instantiation()`（0x1CAA478）建表，**每条曲线 101 项**：

| index | 内容 |
|---|---|
| 0 | `table[n] = n / 100`（Linear） |
| 1..12 | `p = i/3 + 2`；IN=`powf(n/100, p)`，OUT=`1 - powf(1 - n/100, p)`，InOut 见下 |
| 13 | 全 0（HoldStart） |
| 14 | 全 1（JumpToEnd） |

InOut（`i+2`）分两段构造：

```
data[n]     = 0.5 * IN[2n]              for n = 0..49
data[50+k]  = 0.5 * OUT[2k] + 0.5       for k = 0..49
data[100]   = 1.0                       // 显式写死
```

第二段用的是 **OUT** 表（`EaseInfos[i+1]`），代入 `OUT[u] = 1-(1-u)^p`：

```
InOut[50+k] = 1 - 0.5 * (1 - 2k/100)^p
```

令 `u = (50+k)/100`，则 `1 - 2k/100 = 2 - 2u`，于是

```
InOut(u) = 1 - 0.5 * (2 - 2u)^p        // u >= 0.5
InOut(u) = 0.5 * (2u)^p = 2^(p-1) * u^p // u <  0.5
```

**这就是教科书的对称式**（t=0.75、p=2 时给 0.875）。

> 结论：Phira Pro 的 `easing_sample` 正确。
> 早期本地实现声称「官方 InOut 不对称，t=0.75 应为 0.625」，是把第二半段的
> 源表误认成 IN。**该说法错误，已废弃。**

---

## 3. 几何（`PreviewBlockControl`）

### 3.1 `AnchorToWorld(anchor)`（0x1D6D57C）

```
return (anchor - (0.5, 0.5)) * (screenWidth, screenHeight)
```

归一化屏百分比 → 世界像素。换算到本项目 chart 空间（x∈[-1,1]，y∈[-1/aspect,1/aspect]）
即 `pct_to_chart(p) = (2p.x-1, (2p.y-1)/aspect)`，两者只差**统一比例因子**，
而所有用到 anchor 的地方都是仿射组合（`ScaleAroundAnchor` / `RotateAroundAnchor`），
统一因子会约掉，所以 Pro 的归一化写法等价。

### 3.2 `GetBlockGeometry(anchor)`（0x1D6CCC0）

```
tr = AnchorToWorld(topRightPercentage)
bl = AnchorToWorld(bottomLeftPercentage)
size   = tr - bl
center = (tr + bl) * 0.5
```

### 3.3 `UpdateBlockAnimations()`（0x1D6C98C）—— 三条链共用同一套骨架

```
(size, center)  = UpdateScale(originalSize, originalCenter)     // 24_0
(rot,  center2) = UpdateRotation(center)                        // 24_1
finalCenter     = UpdateMovement(originalCenter, center2)
transform.localPosition = (finalCenter.x, finalCenter.y, 0)
transform.localScale    = (fabs(size.x), fabs(size.y), 1)
transform.eulerAngles   = (0, 0, rot)
```

**`fabs` 发生在 `set_localScale`，不是 `UpdateScale` 内部** —— 与 Pro 在
`transform()` 里取 `abs` 等价。

### 3.4 `UpdateScale`（`g__UpdateScale|24_0`，0x1D6CD78）

```
i = FindCurrentEventIndex(scaleEvents, e => e.time)   // 最后一个 time <= t
if (i == -1) return currentSize                       // 无事件 → scale 保持 1

// 1) 重放**已经完成**的事件段
for (k = 0; k < i; k++)
    center = ScaleAroundAnchor(center, AnchorToWorld(e[k].anchor),
                               SafeDiv(e[k+1].scale.x, e[k].scale.x),
                               SafeDiv(e[k+1].scale.y, e[k].scale.y))

// 2) 当前段
cur  = e[i]
if (i+1 < scaleEvents.Count) {
    nxt = e[i+1]
    px = CalculateEasedProgress(cur.time, nxt.time, cur.easeTypeX)
    py = CalculateEasedProgress(cur.time, nxt.time, cur.easeTypeY)
    scale = cur.scale + clamp01(px,py) * (nxt.scale - cur.scale)
} else {
    scale = cur.scale                                 // 最后一帧，直接取
}
if (i+1 < Count)
    center = ScaleAroundAnchor(center, AnchorToWorld(cur.anchor),
                               SafeDiv(scale.x, cur.scale.x),
                               SafeDiv(scale.y, cur.scale.y))

return (scale * originalSize, center)
```

两个要点：

1. **重放用的是关键帧的绝对值** `e[k+1].scale`（等价于 progress=1），不是插值结果。
2. **缓动类型取段的起始事件** `e[i].easeTypeX/Y`（Pro 一致）。
3. 第 1 段（`i==0`）不绕 anchor 移动中心；第一帧之前用默认值。

`ScaleAroundAnchor(point, anchor, stepX, stepY)`（0x1D6D598）：

```
return anchor + (point - anchor) * (stepX, stepY)
```

### 3.5 `UpdateRotation`（`g__UpdateRotation|24_1`，0x1D6D0F4）

同一骨架，`RotateAroundAnchor(center, AnchorToWorld(e[k].anchor), e[k+1].rotation - e[k].rotation)`，
当前段用 `CalculateEasedProgress(..., cur.easeType)` 插值角度。

### 3.6 `UpdateMovement`（0x1D6D404）

```
i = FindCurrentEventIndex(moveEvents, e => e.time)
if (i == -1) return currentCenter
target = (i+1 < Count) ? 插值(e[i], e[i+1], easeX/easeY) : e[i].endPosition
return currentCenter + (AnchorToWorld(target) - originalCenter)
```

即「绝对目标 + 累计位移」，与 `center + (target - originalCenter)` 等价，
也保留了前面 scale/rotate 造成的中心偏移。

### 3.7 `SafeDiv(numerator, denominator)`（0x1D6D6A0）

```
if (|den| < max(1e-6 * |den|, 8 * Epsilon))   // Mathf.Approximately(den, 0)
    return 1.0
return numerator / denominator
```

> ⚠️ Phira Pro 写成 `if b.abs() < f32::from_bits(8) { 1. }`。`f32::from_bits(8)` = 2^-146，
> 只兜住**恰好为 0**（及次正规）的情况；Unity 的阈值是 `max(1e-6*|den|, 8*Mathf.Epsilon)`
> ≈ 9.5e-7。当 `den` 是「很小但不为 0」时 Pro 会得到巨大比值，官方返回 1。
> 我们按官方实现。

### 3.8 `FindCurrentEventIndex`（泛型实例 0x1F962DC / 0x1F96398）

`partition_point(time <= t) - 1`，即「最后一个 time <= t」；t 早于首个事件时返回 -1。

---

## 4. 输入判定（`JudgeControl`）

`inset = maxBlockTouchInsetLocal(0.05) * screenHeight`。

块几何判定用**奇偶规则**：普通块 XOR subtract 块（`origNon != (origSub & 1)`），
且原矩形与 inset 矩形**都要**通过。

> 输入用奇偶、视觉用 subtract 阈值带通（0.09..0.12），二者**故意不同**，不要统一。

---

## 5. 生命周期（`UpdateBlockActivation`，0x1D6C7CC）

```
timeValid = (appearTime <= t) && (t < disappearTime)
isActive  = (enableTime <= t) && (t < disableTime)
readyWin  = (enableTime - disabledBlockReadyDuration <= t) && (t < enableTime)
```

`readyWin` 用的是 **0x5C `disabledBlockReadyDuration`**。

* `IsTimeValid()` 里 `currentTime` 读自 `ProgressControl` + 0x88。
* 该函数只负责切 `GameObject.layer`（enabledLayer / disabledLayer / readyLayer），
  **不是 alpha 渐变**。

### 渐显（`<DisabledBlockShow>d__40.MoveNext`，0x1D6DF90）

`SpriteRenderer.color` 从 startColor 线性插值到 targetColor，时长 =
**0x58 `disabledBlockShowDuration`**，`t/duration` 先 clamp 到 `[0,1]`：
`t = min(t + Time.deltaTime, ...)`；`color = lerp(start, target, clamp01(t/duration))`。

---

## 6. 与 Phira Pro 的差异清单（实现时必须注意）

| 项 | 官方 | Phira Pro | 我们的选择 |
|---|---|---|---|
| InOut 缓动 | `1-0.5(2-2u)^p` | 同 | 同 Pro（正确） |
| `SafeDiv` 阈值 | `max(1e-6·\|d\|, 8ε)` | `2^-146` | **按官方** |
| 渐显时长 | `disabledBlockShowDuration`(0x58) | 0.5 常数 | 按 Pro 的 0.5，代码里注明来源字段 |
| Ready 窗口 | `disabledBlockReadyDuration`(0x5C) | 0.5 常数 | 按 Pro 的 0.5，注明 |
| 生命周期表现 | 切 layer | 用 opacity 渐变建模 Disabled | 按 Pro（视觉等价，待 oracle 差分） |
| `Start()` 无 `blockInfo` 时 | 直接 return（不写 layer 名） | — | 无需处理 |

---

## 7. 实现状态与依赖缺口

代码落脚点：

| 模块 | 作用 |
|---|---|
| `phire/src/core/block.rs` | 结构 + 缓动表 + 变换链 + 触点奇偶判定（13 个单测） |
| `phire/src/core/block_mask.rs` | mask / EdgeMask / GlowMask 的 CPU 等价 |
| `phire/src/core/block_touch.rs` | touch 相机 CPU 源（hover） |
| `phire/src/core/block_shader.rs` | 全屏材质 + 帧内缓存 + 两层级接入 |
| `phire/src/core/shaders/block_full.{glsl,vert}` | 官方 GLSL（逐字符保留） |
| `phire/src/parse/pgr.rs` | `blockAreaList` 反序列化 |
| `phire/src/judge.rs` | 触点拦截 + `infected` finger ID |
| `phire/src/core/chart.rs` / `scene/game.rs` | 渲染层级接线 |

### 7.1 两处 API 缺口（本仓库的 macroquad/miniquad fork 与 Pro 的不同）

1. **`glCopyTexSubImage2D`**：Pro 靠自己的 `vendor/prpr-miniquad` 补了这个符号，
   本仓库 pin 的 `2278535805/miniquad` 原生端**没有导出**。
   → 改用谱面 render target 的双缓冲（交换后从旧图采样、往新图写），
   与 `core/effect.rs` 的后处理同一条路径，不需要额外的 framebuffer 拷贝。
2. **`Texture2D::set_wrap`**：本仓库的 macroquad fork 没有它。
   → 改用 miniquad 的 `RenderingBackend::texture_set_wrap`。

### 7.2 array uniform 的坑（已被冒烟测试抓到）

`_TouchPos` 是 `uniform vec2 _TouchPos[10]`。miniquad 把它当作**一个** array uniform：
`glGetUniformLocation("_TouchPos")` + `glUniform2fv(loc, 10, data)`。
按 `_TouchPos[i]` 逐个 `set_uniform` 会**全部落空**（只打 “non-existing uniform” 警告，
不报错），hover 的触点坐标会一直是 0。必须用 `Material::set_uniform_array`。

> 这类问题 `cargo check` 看不出来，所以有
> `cargo run -p phire --example block_shader_smoke` —— 它在真实 GL 驱动上链接材质并
> 走一遍绘制路径。

### 7.3 尚未做

* **音频低通**：官方在首次有触点被遮挡时给谱面音乐挂 1500 Hz 低通、0.1s 过渡到 22000 Hz。
  Pro 为此改了自己的 sasa fork（二阶共振）。本仓库 pin 的 sasa 只有一阶
  `set_low_pass` → 未实现。
* **像素差分**：可以用 `phi-recorder.exe`（Phira Pro 的可运行实现）对同一谱面同一时刻出帧
  做差分，定位 Pro 自己承认仍未解决的整帧色调差。
* **iOS**：Metal 后端需要单独验证。

