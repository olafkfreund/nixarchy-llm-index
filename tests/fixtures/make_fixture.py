"""Build the fixture tree: copy tree/, then add what cannot live in git (binary formats,
denied and ignored files, a symlink, a zip bomb, a very long HTML line) and set mtimes.
Usage: make_fixture.py <tree> <out>"""
import os
import shutil
import sys
import zipfile

src, out = sys.argv[1:3]
shutil.copytree(src, out, symlinks=True)
os.chmod(out, 0o755)
for d, dirs, files in os.walk(out):
    for n in dirs:
        os.chmod(os.path.join(d, n), 0o755)
    for n in files:
        os.chmod(os.path.join(d, n), 0o644)


def put(rel, data, mode="w"):
    path = os.path.join(out, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, mode) as f:
        f.write(data)


def make_zip(rel, members, bomb=None):
    path = os.path.join(out, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with zipfile.ZipFile(path, "w") as z:
        for name, data in members:
            comp = zipfile.ZIP_STORED if name == "mimetype" else zipfile.ZIP_DEFLATED
            z.writestr(zipfile.ZipInfo(name), data, compress_type=comp)
        if bomb:
            name, chunk, total = bomb
            zi = zipfile.ZipInfo(name)
            zi.compress_type = zipfile.ZIP_DEFLATED
            with z.open(zi, "w", force_zip64=True) as w:
                for _ in range(total // len(chunk)):
                    w.write(chunk)


# denied and ignored files, a symlink to a denied file
put(".env", "SECRET_TOKEN=envleakmarker\n")
put(".netrc", "machine example login netrcleakmarker\n")
os.symlink(".env", os.path.join(out, "docs/link-to-env.txt"))
put("repo/ignored.txt", "ignoredmarker should never be indexed\n")
put("repo/build-output/out.txt", "ignoredmarker build artefact\n")

# a long-line HTML page (one 300 kB line)
put("web/longline.html", "<html><head><title>Long line page</title></head><body><p>" +
    "harbourlighthouse " * 16000 + "</p></body></html>")


def pdf(text):
    stream = f"BT /F1 12 Tf 72 720 Td ({text}) Tj ET"
    objs = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R "
        "/Resources << /Font << /F1 5 0 R >> >> >>",
        f"<< /Length {len(stream)} >>\nstream\n{stream}\nendstream",
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        "<< /Title (Annual financial report) >>",
    ]
    body, offsets = "%PDF-1.4\n", []
    for i, o in enumerate(objs, 1):
        offsets.append(len(body))
        body += f"{i} 0 obj\n{o}\nendobj\n"
    xref = len(body)
    body += f"xref\n0 {len(objs) + 1}\n0000000000 65535 f \n"
    body += "".join(f"{o:010d} 00000 n \n" for o in offsets)
    body += f"trailer\n<< /Size {len(objs) + 1} /Root 1 0 R /Info 6 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    return body


put("docs/report.pdf", pdf("Revenue increased in the third quarter thanks to cloud subscriptions and fewer refunds."))

CT = ('<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
      '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
      '<Default Extension="xml" ContentType="application/xml"/>%s</Types>')
RELS = ('<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="%s"/>'
        '<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/></Relationships>')
CORE = ('<?xml version="1.0" encoding="UTF-8"?><cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" '
        'xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>%s</dc:title></cp:coreProperties>')
W = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
DOCX_CT = "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"


def docx_body(text, n=1):
    return (f'<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="{W}"><w:body>' +
            f"<w:p><w:r><w:t>{text}</w:t></w:r></w:p>" * n + "</w:body></w:document>")


make_zip("docs/contract.docx", [
    ("[Content_Types].xml", CT % f'<Override PartName="/word/document.xml" ContentType="{DOCX_CT}"/>'),
    ("_rels/.rels", RELS % "word/document.xml"),
    ("docProps/core.xml", CORE % "Service agreement"),
    ("word/document.xml", docx_body("Either party may terminate the agreement with ninety days written notice. "
                                    "Liability is limited to the fees paid in the previous year.")),
])

# zip bomb: a docx whose main part inflates to 600 MB
bomb_chunk = (f"<w:p><w:r><w:t>bombmarker</w:t></w:r></w:p>").encode() * 1000
make_zip("docs/bomb.docx", [
    ("[Content_Types].xml", CT % f'<Override PartName="/word/document.xml" ContentType="{DOCX_CT}"/>'),
    ("_rels/.rels", RELS % "word/document.xml"),
], bomb=("word/document.xml", bomb_chunk, 600 * 1024 * 1024))

ODF = ('xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" '
       'xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/" '
       'xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0"')
MANIFEST = ('<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2">'
            '<manifest:file-entry manifest:full-path="/" manifest:media-type="%s"/>'
            '<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>')


def odf(rel, mime, title, body):
    make_zip(rel, [
        ("mimetype", mime),
        ("META-INF/manifest.xml", MANIFEST % mime),
        ("meta.xml", f'<?xml version="1.0" encoding="UTF-8"?><office:document-meta {ODF}><office:meta><dc:title>{title}</dc:title></office:meta></office:document-meta>'),
        ("content.xml", f'<?xml version="1.0" encoding="UTF-8"?><office:document-content {ODF}><office:body>{body}</office:body></office:document-content>'),
    ])


odf("docs/manual.odt", "application/vnd.oasis.opendocument.text", "Espresso machine manual",
    '<office:text><text:p>Descale the boiler monthly and grind the beans fine for a pressure of nine bar.</text:p></office:text>')


def ods_row(*cells):
    return "<table:table-row>" + "".join(
        f'<table:table-cell office:value-type="string"><text:p>{c}</text:p></table:table-cell>' for c in cells) + "</table:table-row>"


odf("data/members.ods", "application/vnd.oasis.opendocument.spreadsheet", "Chess club members",
    '<office:spreadsheet><table:table table:name="Members">' + ods_row("member", "rating") +
    ods_row("Magnus", "2850") + ods_row("Judit", "2735") + "</table:table></office:spreadsheet>")

SHEET_CT = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"
S = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"


def cell(ref, v):
    return f'<c r="{ref}" t="inlineStr"><is><t>{v}</t></is></c>'


make_zip("data/sales.xlsx", [
    ("[Content_Types].xml", CT % (f'<Override PartName="/xl/workbook.xml" ContentType="{SHEET_CT}"/>'
                                  '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>')),
    ("_rels/.rels", RELS % "xl/workbook.xml"),
    ("docProps/core.xml", CORE % "Regional sales"),
    ("xl/workbook.xml", f'<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="{S}" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Regions" sheetId="1" r:id="rId1"/></sheets></workbook>'),
    ("xl/_rels/workbook.xml.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>'),
    ("xl/worksheets/sheet1.xml", f'<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="{S}"><sheetData>'
     f'<row r="1">{cell("A1", "region")}{cell("B1", "turnover")}</row>'
     f'<row r="2">{cell("A2", "Scandinavia")}{cell("B2", "huge")}</row>'
     f'<row r="3">{cell("A3", "Patagonia")}{cell("B3", "modest")}</row></sheetData></worksheet>'),
])

# mtimes: the dated files are old, the rest recent (2026-06-01)
recent = 1780272000
old = 1577836800  # 2020-01-01
for d, _, files in os.walk(out):
    for n in files:
        p = os.path.join(d, n)
        if not os.path.islink(p):
            os.chmod(p, 0o644)
            os.utime(p, (recent, recent))
for rel in ["docs/travel-japan.md", "docs/astronomy.md", "docs/sourdough.md"]:
    os.utime(os.path.join(out, rel), (old, old))
