"""Regenerate synthetic fixtures with python-docx, XlsxWriter, python-pptx and reportlab."""
from datetime import datetime, timezone
from pathlib import Path
from docx import Document
from docx.oxml import parse_xml
from pptx import Presentation
from pptx.util import Inches
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.cidfonts import UnicodeCIDFont
from reportlab.pdfgen import canvas
import xlsxwriter

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
doc._element.body.insert(-1, parse_xml(
    '<w:p xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" '
    'xmlns:v="urn:schemas-microsoft-com:vml"><w:r><w:pict>'
    '<v:shape style="width:100pt;height:30pt" type="#_x0000_t202"><v:textbox>'
    '<w:txbxContent><w:p><w:r><w:t>合成文本框火星</w:t></w:r></w:p></w:txbxContent>'
    '</v:textbox></v:shape></w:pict></w:r></w:p>'
))
doc.save(root / "synthetic.docx")

book = xlsxwriter.Workbook(root / "synthetic.xlsx")
book.set_properties({"author": "LLMLocalSearch", "created": fixed_time.replace(tzinfo=None)})
sheet = book.add_worksheet("合成表格")
sheet.write_string("A1", "搜索橘子")  # Stored in the shared-string table.
sheet.write_formula("B1", "=937240+1", None, 937241)  # Cached value, no recalculation.
book.close()

slides = Presentation()
slides.core_properties.author = "LLMLocalSearch"
slides.core_properties.last_modified_by = "LLMLocalSearch"
slides.core_properties.created = fixed_time
slides.core_properties.modified = fixed_time
slide = slides.slides.add_slide(slides.slide_layouts[6])
slide.shapes.add_textbox(Inches(1), Inches(1), Inches(6), Inches(1)).text = "合成PPT 搜索葡萄"
slide.notes_slide.notes_text_frame.text = "合成幻灯备注海王星"
slides.save(root / "synthetic.pptx")

pdfmetrics.registerFont(UnicodeCIDFont("STSong-Light"))
pdf = canvas.Canvas(str(root / "synthetic.pdf"), invariant=1)
pdf.setAuthor("LLMLocalSearch")
pdf.setTitle("Synthetic fixture")
pdf.setFont("STSong-Light", 18)
pdf.drawString(50, 750, "PDF合成文本 搜索西瓜")
pdf.save()
