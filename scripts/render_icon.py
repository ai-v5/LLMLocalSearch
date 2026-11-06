"""Render the project icon at multiple sizes. Requires Pillow for regeneration only."""
from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
S = 1024
image = Image.new("RGBA", (S, S))
draw = ImageDraw.Draw(image)
scale = S / 256

def points(values):
    return [(int(x * scale), int(y * scale)) for x, y in values]

def box(values):
    return tuple(int(x * scale) for x in values)

draw.rounded_rectangle(box((8, 8, 248, 248)), radius=int(54 * scale), fill="#142c50")
draw.rounded_rectangle(box((50, 42, 180, 194)), radius=int(13 * scale), fill="white")
draw.polygon(points([(142,42),(180,80),(180,42)]), fill="#142c50")
draw.polygon(points([(142,42),(142,80),(180,80)]), fill="#b7ccda")
for start, end in [((75,99),(142,99)),((75,120),(132,120)),((75,141),(117,141))]:
    draw.line(points([start,end]), fill="#39cdb4", width=int(9 * scale))
    for x,y in (start,end):
        draw.ellipse(box((x-4.5,y-4.5,x+4.5,y+4.5)), fill="#39cdb4")
draw.ellipse(box((118,112,198,192)), fill="#39cdb4")
draw.ellipse(box((130,124,186,180)), fill="#142c50")
draw.line(points([(182,178),(209,205)]), fill="#39cdb4", width=int(15 * scale))
for x,y in [(182,178),(209,205)]:
    draw.ellipse(box((x-7.5,y-7.5,x+7.5,y+7.5)), fill="#39cdb4")
png = image.resize((256,256), Image.Resampling.LANCZOS)
png.save(ROOT / "assets" / "icon.png")
png.save(ROOT / "assets" / "icon.ico", sizes=[(16,16),(20,20),(24,24),(32,32),(48,48),(64,64),(128,128),(256,256)])
