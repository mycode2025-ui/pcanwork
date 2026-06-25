# -*- coding: utf-8 -*-
"""生成 modbus-tools 的 app.ico —— 与 pcanwork 同款蓝渐变圆角方块品牌,
内白色 Modbus 寄存器阶梯折线。超采样绘制后下采样到多尺寸 ICO。"""
from PIL import Image, ImageDraw

SS = 1024  # 超采样画布


def s(v, base=128):
    """把 pcanwork 128 视图坐标缩放到超采样画布。"""
    return v / base * SS


def make_gradient(size, c0, c1):
    """对角线渐变 c0(左上) → c1(右下)。"""
    g = Image.new("RGB", (size, size))
    px = g.load()
    span = 2 * (size - 1)
    for y in range(size):
        for x in range(size):
            t = (x + y) / span
            px[x, y] = tuple(round(c0[i] + (c1[i] - c0[i]) * t) for i in range(3))
    return g


# 背景渐变 + 圆角方块遮罩(版式与 pcanwork ui/logo.svg 一致: inset6 size116 rx26 @128)
grad = make_gradient(SS, (0x1C, 0x5A, 0xA6), (0x11, 0x40, 0x6F)).convert("RGBA")
mask = Image.new("L", (SS, SS), 0)
inset, side, rad = s(6), s(116), s(26)
ImageDraw.Draw(mask).rounded_rectangle(
    [inset, inset, inset + side, inset + side], radius=rad, fill=255
)
icon = Image.new("RGBA", (SS, SS), (0, 0, 0, 0))
icon.paste(grad, (0, 0), mask)

# 白色 Modbus 寄存器折线(modbus/ui/logo.svg 的 256 视图坐标 → 缩放到 128 → 超采样)
draw = ImageDraw.Draw(icon)
pts256 = [(48, 168), (92, 168), (92, 104), (136, 104),
          (136, 152), (180, 152), (180, 88), (208, 88)]
pts = [(s(x / 2), s(y / 2)) for (x, y) in pts256]  # 256→128 再到 SS
lw = s(16 / 2)  # 线宽 16@256 → 8@128
draw.line(pts, fill=(255, 255, 255, 255), width=int(lw), joint="curve")
# 圆角端帽/拐角: 每个顶点补一个白色圆(半径=线宽/2),模拟 round cap/join
r = lw / 2
for (x, y) in pts:
    draw.ellipse([x - r, y - r, x + r, y + r], fill=(255, 255, 255, 255))
# 端点的强调圆点(11@256 → 5.5@128)
rdot = s(11 / 2)
for (x, y) in (pts[0], pts[-1]):
    draw.ellipse([x - rdot, y - rdot, x + rdot, y + rdot], fill=(255, 255, 255, 255))

# 下采样导出多尺寸 ICO(Explorer/任务栏各档都清晰)
sizes = [256, 128, 64, 48, 32, 16]
frames = [icon.resize((n, n), Image.LANCZOS) for n in sizes]
frames[0].save("app.ico", format="ICO", sizes=[(n, n) for n in sizes])
# 同时存一张 png 便于预览
frames[0].save("app.png")
print("written modbus/app.ico + app.png, sizes:", sizes)
