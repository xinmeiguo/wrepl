# -*- coding: utf-8 -*-
"""生成 wrepl 的应用图标：熊猫脸。

来源：这个图形是从早先的 `WordHeaderToolV2` 项目原样搬过来的（当时的
`make_icon.py`），只改了输出文件名与落盘位置 —— 保持同一个视觉身份。

产出两样东西，各有用处：

- `wrepl.ico`：交给 `build.rs` 编进 exe 资源节，是**资源管理器 / 任务栏 /
  开始菜单**看到的那个图标。含 16/24/32/48/64/128/256 七个尺寸，Windows
  按显示场景自己挑，缺哪个尺寸它就缩放（会糊）。
- `wrepl-64.rgba`：`src/gui/main.rs` 用 `include_bytes!` 内嵌，开窗时喂给
  `ViewportBuilder::with_icon`。**格式是 8 字节头 + 裸 RGBA**：头是两个
  u32 小端（宽、高），后面紧接 `宽*高*4` 字节。不走 PNG 是为了不给 GUI
  多拉一个解码器依赖 —— 64×64×4 只有 16 KB，而多一个 crate 不止这么点。

用 2048 超采样绘制后缩到 256，边缘才干净。透明背景。
用法：`python assets/make_icon.py`
"""

import os
import struct

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))

S = 2048
cx, cy = 1024, 1052  # 头心略低于画布中心，给耳朵留位

BLACK = (18, 18, 18, 255)
WHITE = (255, 255, 255, 255)
OUTLINE = (70, 70, 70, 255)

img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

# ---- 头部：白色圆 + 细灰描边 ----
R = 760
d.ellipse([cx - R, cy - R, cx + R, cy + R], fill=WHITE, outline=OUTLINE, width=14)

# ---- 耳朵：两个黑色圆，与头顶相切 ----
er = 282
ex = 548
ey = 560
d.ellipse([cx - ex - er, cy - ey - er, cx - ex + er, cy - ey + er], fill=BLACK)
d.ellipse([cx + ex - er, cy - ey - er, cx + ex + er, cy - ey + er], fill=BLACK)


# ---- 眼眶（黑色斜椭圆）：旋转贴片 ----
def tilted_ellipse(size, angle_deg, fill):
    w, h = size
    layer = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ld = ImageDraw.Draw(layer)
    ld.ellipse([cx - w // 2, cy - h // 2, cx + w // 2, cy + h // 2], fill=fill)
    return layer.rotate(angle_deg, center=(cx, cy))


patch = tilted_ellipse((230, 360), -22, BLACK)
# 左眼眶偏左上，右眼眶偏右上（关于中线镜像）
patch_l = Image.new("RGBA", (S, S), (0, 0, 0, 0))
patch_l.paste(patch, (-190, -40), patch)
img.alpha_composite(patch_l)
patch_r = Image.new("RGBA", (S, S), (0, 0, 0, 0))
patch_r.paste(patch, (190, -40), patch)
img.alpha_composite(patch_r)

# ---- 眼白 ----
eye_r = 92
leye = (cx - 150, cy + 70)
reye = (cx + 150, cy + 70)
d.ellipse([leye[0] - eye_r, leye[1] - eye_r, leye[0] + eye_r, leye[1] + eye_r], fill=WHITE)
d.ellipse([reye[0] - eye_r, reye[1] - eye_r, reye[0] + eye_r, reye[1] + eye_r], fill=WHITE)

# ---- 瞳孔（略偏内下，更生动）----
pr = 56
d.ellipse([leye[0] - pr + 18, leye[1] - pr + 22, leye[0] + pr + 18, leye[1] + pr + 22], fill=BLACK)
d.ellipse([reye[0] - pr - 18, reye[1] - pr + 22, reye[0] + pr - 18, reye[1] + pr + 22], fill=BLACK)

# ---- 高光 ----
hl = 18
d.ellipse([leye[0] + 6, leye[1] - 4, leye[0] + 6 + hl, leye[1] - 4 + hl], fill=WHITE)
d.ellipse([reye[0] - 24, reye[1] - 4, reye[0] - 24 + hl, reye[1] - 4 + hl], fill=WHITE)

# ---- 鼻子：黑色圆角椭圆 ----
nx, ny = cx, cy + 250
nw, nh = 130, 96
d.rounded_rectangle([nx - nw // 2, ny - nh // 2, nx + nw // 2, ny + nh // 2], radius=46, fill=BLACK)

# ---- 嘴：鼻下短竖线 + 两弧（微笑）----
d.line([nx, ny + nh // 2, nx, ny + nh // 2 + 70], fill=BLACK, width=18)
mouth_top = ny + nh // 2 + 70
d.arc([nx - 150, mouth_top, nx, mouth_top + 150], 320, 360, fill=BLACK, width=18)
d.arc([nx, mouth_top, nx + 150, mouth_top + 150], 180, 220, fill=BLACK, width=18)

# ---- 导出 ----
icon = img.resize((256, 256), Image.LANCZOS)
ico_path = os.path.join(HERE, "wrepl.ico")
# ★ 不要加 `optimize=True`：Pillow 那条路径反而把 256 帧压得更大
#   （16.9 KB vs 15.8 KB）。这里的每个帧都是 PNG 压缩的（Vista 起 ICO 支持），
#   黑白图天然压得好，七个尺寸合计 33 KB。
icon.save(
    ico_path,
    format="ICO",
    sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
)

# 窗口图标：64×64 裸 RGBA，前面顶 8 字节头（宽、高，u32 小端）
w = 64
small = img.resize((w, w), Image.LANCZOS)
rgba_path = os.path.join(HERE, "wrepl-64.rgba")
with open(rgba_path, "wb") as f:
    f.write(struct.pack("<II", w, w))
    f.write(small.tobytes())

for p in (ico_path, rgba_path):
    print("%-28s %7d 字节" % (os.path.basename(p), os.path.getsize(p)))
