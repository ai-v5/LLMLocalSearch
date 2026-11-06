"""Regenerate synthetic integration fixtures; requires python-docx, openpyxl, python-pptx and reportlab."""
from datetime import datetime, timezone
from pathlib import Path
from docx import Document
from openpyxl import Workbook
from pptx import Presentation
from pptx.util import Inches
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.cidfonts import UnicodeCIDFont
from reportlab.pdfgen import canvas

root = Path(__file__).resolve().parent / "fixtures"
root.mkdir(exist_ok=True)
fixed_time = datetime(2020, 1, 1, tzinfo=timezone.utc)

doc = Document()
doc.core_properties.author = "LLMLocalSearch"
doc.core_properties.last_modified_by = "LLMLocalSearch"
doc.core_properties.created = fixed_time
doc.core_properties.modified = fixed_time
doc.add_paragraph("合成Word文档 搜索苹果")
doc.add_table(rows=1, cols=1).cell(0, 0).text = "表格内容香蕉"
doc.save(root / "synthetic.docx")

book = Workbook()
book.properties.creator = "LLMLocalSearch"
book.properties.lastModifiedBy = "LLMLocalSearch"
book.properties.created = fixed_time.replace(tzinfo=None)
book.properties.modified = fixed_time.replace(tzinfo=None)
sheet = book.active
sheet.title = "合成表格"
sheet["A1"] = "搜索橘子"
sheet["B1"] = 42
book.save(root / "synthetic.xlsx")

slides = Presentation()
slides.core_properties.author = "LLMLocalSearch"
slides.core_properties.last_modified_by = "LLMLocalSearch"
slides.core_properties.created = fixed_time
slides.core_properties.modified = fixed_time
slide = slides.slides.add_slide(slides.slide_layouts[6])
slide.shapes.add_textbox(Inches(1), Inches(1), Inches(6), Inches(1)).text = "合成PPT 搜索葡萄"
slides.save(root / "synthetic.pptx")

pdfmetrics.registerFont(UnicodeCIDFont("STSong-Light"))
pdf = canvas.Canvas(str(root / "synthetic.pdf"), invariant=1)
pdf.setAuthor("LLMLocalSearch")
pdf.setTitle("Synthetic fixture")
pdf.setFont("STSong-Light", 18)
pdf.drawString(50, 750, "PDF合成文本 搜索西瓜")
pdf.save()
