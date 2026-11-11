{
  pkgs,
  nixpkgs,
}:
let
  gutenbergBooks = [
    {
      id = "pg1342";
      title = "Pride and Prejudice";
      file = pkgs.fetchurl {
        url = "https://www.gutenberg.org/cache/epub/1342/pg1342.txt";
        hash = "sha256-P2u51veOApO1as1HFN1oy31tHSk0AgMc6dWiFryvnXU=";
      };
    }
    {
      id = "pg11";
      title = "Alices Adventures in Wonderland";
      file = pkgs.fetchurl {
        url = "https://www.gutenberg.org/cache/epub/11/pg11.txt";
        hash = "sha256-AbOOpMcQqEvBjQvUEnGloakrlOl7KBL03s6X1KaUcl4=";
      };
    }
    {
      id = "pg84";
      title = "Frankenstein";
      file = pkgs.fetchurl {
        url = "https://www.gutenberg.org/cache/epub/84/pg84.txt";
        hash = "sha256-K5e3+KrVCGSUgdjdT2tU+bll3bO6Y06RUcgV9Te0nkI=";
      };
    }
    {
      id = "pg1661";
      title = "The Adventures of Sherlock Holmes";
      file = pkgs.fetchurl {
        url = "https://www.gutenberg.org/cache/epub/1661/pg1661.txt";
        hash = "sha256-ki4qEsy0OkyVRMJgshZsatIJeutZV/ru4RPxc7uFfNA=";
      };
    }
    {
      id = "pg2701";
      title = "Moby Dick";
      file = pkgs.fetchurl {
        url = "https://www.gutenberg.org/cache/epub/2701/pg2701.txt";
        hash = "sha256-teiLcaHNoGjpqx2UNEydkyM4um2hpm3vl6GuzZz8yH0=";
      };
    }
    {
      id = "pg98";
      title = "A Tale of Two Cities";
      file = pkgs.fetchurl {
        url = "https://www.gutenberg.org/cache/epub/98/pg98.txt";
        hash = "sha256-1UwrgNQKQLmCzYiFLGGAu5RNlazbAorz0OAaF1BoF4Q=";
      };
    }
    {
      id = "pg1952";
      title = "The Yellow Wallpaper";
      file = pkgs.fetchurl {
        url = "https://www.gutenberg.org/cache/epub/1952/pg1952.txt";
        hash = "sha256-IMd4juri/joO/VGh8ItnIzZqS+B3jWXbIZFJo1ImUdQ=";
      };
    }
    {
      id = "pg174";
      title = "The Picture of Dorian Gray";
      file = pkgs.fetchurl {
        url = "https://www.gutenberg.org/cache/epub/174/pg174.txt";
        hash = "sha256-OPNrUQQXF3qoemokyWjj7GOkR7ffNqnBp8Wj8tnlFUc=";
      };
    }
    {
      id = "pg345";
      title = "Dracula";
      file = pkgs.fetchurl {
        url = "https://www.gutenberg.org/cache/epub/345/pg345.txt";
        hash = "sha256-ls0W6s2/666P3aVZH2bgzI7na+GODNGsoCvABhV4LSg=";
      };
    }
    {
      id = "pg1260";
      title = "Jane Eyre";
      file = pkgs.fetchurl {
        url = "https://www.gutenberg.org/cache/epub/1260/pg1260.txt";
        hash = "sha256-E0FN7ilRw+5zHXbS/9giAWskeciSFidgxdDrKqX6djE=";
      };
    }
  ];

  pythonEnv = pkgs.python3.withPackages (ps: [
    ps.openpyxl
    ps.odfpy
  ]);

  booksJson = pkgs.writeText "gutenberg-books.json" (builtins.toJSON gutenbergBooks);

  builderScript = pkgs.writeText "build-bench-corpus.py" ''
    import csv
    import datetime
    import io
    import json
    import os
    import re
    import shutil
    import subprocess
    import sys
    import tempfile
    import zipfile
    from concurrent.futures import ThreadPoolExecutor

    import openpyxl
    from odf.opendocument import OpenDocumentSpreadsheet
    from odf.table import Table, TableCell, TableRow
    from odf.text import P

    os.environ["SOURCE_DATE_EPOCH"] = "315532800"

    OUT = os.environ["out"]
    NIXPKGS = os.environ["NIXPKGS_SRC"]
    BOOKS_JSON = os.environ["BOOKS_JSON"]
    WORKERS = min(int(os.environ.get("NIX_BUILD_CORES", "0")) or (os.cpu_count() or 8), 64)

    CODE_EXTS = {
        ".rs", ".py", ".pyi", ".go", ".js", ".mjs", ".cjs", ".jsx",
        ".ts", ".mts", ".cts", ".tsx", ".nix", ".c", ".h", ".cc",
        ".cpp", ".cxx", ".hpp", ".hh", ".hxx", ".java", ".sh",
        ".bash", ".toml", ".yaml", ".yml", ".json", ".jsonc", ".map", ".css",
    }
    MINIFIED_EXTS = {".js", ".css", ".json", ".map"}
    FIXED_DT = (1980, 1, 1, 0, 0, 0)


    def normalize_zip_bytes(raw_bytes: bytes, is_odf: bool = False) -> bytes:
        in_zf = zipfile.ZipFile(io.BytesIO(raw_bytes), "r")
        out_buf = io.BytesIO()
        with zipfile.ZipFile(out_buf, "w") as out_zf:
            names = in_zf.namelist()
            if is_odf and "mimetype" in names:
                names.remove("mimetype")
                names = ["mimetype"] + sorted(names)
            elif "[Content_Types].xml" in names:
                names.remove("[Content_Types].xml")
                names = ["[Content_Types].xml"] + sorted(names)
            else:
                names = sorted(names)
            for name in names:
                data = in_zf.read(name)
                if name == "docProps/core.xml":
                    data = re.sub(
                        rb"<dcterms:modified[^>]*>[^<]*</dcterms:modified>",
                        b'<dcterms:modified xsi:type="dcterms:W3CDTF">1980-01-01T00:00:00Z</dcterms:modified>',
                        data,
                    )
                    data = re.sub(
                        rb"<dcterms:created[^>]*>[^<]*</dcterms:created>",
                        b'<dcterms:created xsi:type="dcterms:W3CDTF">1980-01-01T00:00:00Z</dcterms:created>',
                        data,
                    )
                elif name == "meta.xml":
                    data = re.sub(
                        rb"<meta:creation-date>[^<]*</meta:creation-date>",
                        b"<meta:creation-date>1980-01-01T00:00:00Z</meta:creation-date>",
                        data,
                    )
                    data = re.sub(
                        rb"<dc:date>[^<]*</dc:date>",
                        b"<dc:date>1980-01-01T00:00:00Z</dc:date>",
                        data,
                    )
                zi = zipfile.ZipInfo(filename=name, date_time=FIXED_DT)
                zi.external_attr = 0o644 << 16
                if is_odf and name == "mimetype":
                    zi.compress_type = zipfile.ZIP_STORED
                else:
                    zi.compress_type = zipfile.ZIP_DEFLATED
                out_zf.writestr(zi, data)
        return out_buf.getvalue()


    def normalize_zip_file(path: str, is_odf: bool = False) -> None:
        with open(path, "rb") as fp:
            raw = fp.read()
        norm = normalize_zip_bytes(raw, is_odf=is_odf)
        with open(path, "wb") as fp:
            fp.write(norm)


    def load_paragraphs() -> list[tuple[str, str]]:
        with open(BOOKS_JSON, "r", encoding="utf-8") as fp:
            books = json.load(fp)
        paras: list[tuple[str, str]] = []
        for book in books:
            title = book["title"]
            with open(book["file"], "r", encoding="utf-8", errors="replace") as fp:
                raw = fp.read()
            for block in raw.split("\n\n"):
                cleaned = " ".join(block.split())
                # Keep ASCII printable words and basic punctuation so pandoc/typst never choke
                cleaned = re.sub(r"[^A-Za-z0-9 ,.;:!?()'\"-]", " ", cleaned)
                cleaned = " ".join(cleaned.split())
                if 80 <= len(cleaned) <= 900:
                    paras.append((title, cleaned))
        if len(paras) < 2000:
            raise RuntimeError(f"Expected >= 2000 paragraphs, got {len(paras)}")
        return paras


    def pick_code_files(limit: int = 4000) -> list[tuple[str, str, str]]:
        candidates: list[tuple[str, str, str]] = []
        for root, dirs, files in os.walk(NIXPKGS, followlinks=False):
            dirs[:] = sorted(
                d for d in dirs if not os.path.islink(os.path.join(root, d))
            )
            for fname in sorted(files):
                full = os.path.join(root, fname)
                if os.path.islink(full) or not os.path.isfile(full):
                    continue
                _, ext = os.path.splitext(fname)
                ext_l = ext.lower()
                if ext_l not in CODE_EXTS:
                    continue
                rel = os.path.relpath(full, NIXPKGS)
                candidates.append((rel, full, ext_l))

        candidates.sort(key=lambda item: item[0])
        selected: list[tuple[str, str, str]] = []
        for rel, full, ext_l in candidates:
            try:
                st = os.stat(full)
                if st.st_size == 0 or st.st_size > 256 * 1024:
                    continue
                with open(full, "rb") as fp:
                    raw = fp.read()
                if b"\x00" in raw[:8192]:
                    continue
                text = raw.decode("utf-8")
                if not text.strip():
                    continue
                head_4k = text[:4096]
                if "@generated" in head_4k:
                    continue
                pos = head_4k.find("Code generated")
                if pos != -1 and "DO NOT EDIT" in head_4k[pos:]:
                    continue
                if ext_l in MINIFIED_EXTS:
                    lines = text.splitlines()
                    nlines = max(len(lines), 1)
                    if sum(len(l) for l in lines) > 250 * nlines:
                        continue
                selected.append((rel, full, ext_l))
                if len(selected) == limit:
                    break
            except Exception:
                continue

        if len(selected) != limit:
            raise RuntimeError(f"Expected {limit} code files, found {len(selected)}")
        return selected


    def make_markdown_source(doc_id: int, kind: str, paras: list[tuple[str, str]]) -> tuple[str, str]:
        n = len(paras)
        base = (doc_id * 7) % n
        book_title = paras[base][0]
        title = f"{book_title} {kind} Document {doc_id:04d}"
        lines = [f"# {title}", ""]
        for sec in range(1, 5):
            p1 = paras[(base + sec * 3) % n][1]
            p2 = paras[(base + sec * 3 + 1) % n][1]
            lines.append(f"## Section {sec} of Document {doc_id:04d}")
            lines.append("")
            lines.append(f"{p1} (corpus-{kind}-{doc_id:04d}-s{sec})")
            lines.append("")
            lines.append(p2)
            lines.append("")
        return title, "\n".join(lines)


    def make_typst_source(doc_id: int, paras: list[tuple[str, str]]) -> str:
        n = len(paras)
        base = (doc_id * 11) % n
        book_title = paras[base][0]
        title = f"{book_title} PDF Study {doc_id:04d}"
        pages: list[str] = []
        for page_no in range(1, 11):
            p1 = paras[(base + page_no * 2) % n][1]
            p2 = paras[(base + page_no * 2 + 1) % n][1]
            # Strip characters that have markup meaning in Typst
            for ch in ("#", "$", "*", "_", "`", "<", ">", "@", "\\", "[", "]", "=", "+", "/", "~"):
                p1 = p1.replace(ch, " ")
                p2 = p2.replace(ch, " ")
            header = f"= {title}\n\n" if page_no == 1 else ""
            section = (
                f"{header}== Page {page_no} of Study {doc_id:04d}\n\n"
                f"Reference token: corpus-pdf-{doc_id:04d}-p{page_no}.\n\n"
                f"{p1}\n\n{p2}\n"
            )
            pages.append(section)
        return "\n#pagebreak()\n\n".join(pages)


    def main() -> None:
        os.makedirs(OUT, exist_ok=True)
        paras = load_paragraphs()

        # 1. 4,000 code files from pinned nixpkgs
        code_files = pick_code_files(4000)
        for idx, (rel, full, ext_l) in enumerate(code_files):
            bucket = f"{idx // 100:02d}"
            dest_dir = os.path.join(OUT, "code", bucket)
            os.makedirs(dest_dir, exist_ok=True)
            stem = os.path.splitext(os.path.basename(rel))[0]
            safe_stem = re.sub(r"[^A-Za-z0-9_-]", "_", stem)[:40] or "src"
            dest = os.path.join(dest_dir, f"{idx:04d}-{safe_stem}{ext_l}")
            shutil.copyfile(full, dest)

        # 2. Pandoc & Typst jobs for Markdown (1,000), Text (1,000), HTML (1,000),
        #    DOCX (500), ODT (500), PDF (1,500)
        with tempfile.TemporaryDirectory() as tmp:
            tasks = []

            for i in range(1000):
                bucket = f"{i // 100:02d}"
                out_dir = os.path.join(OUT, "markdown", bucket)
                os.makedirs(out_dir, exist_ok=True)
                _, src = make_markdown_source(i, "md", paras)
                src_path = os.path.join(tmp, f"md-{i:04d}.md")
                with open(src_path, "w", encoding="utf-8") as fp:
                    fp.write(src)
                dst_path = os.path.join(out_dir, f"doc-{i:04d}.md")
                tasks.append((["pandoc", "-f", "markdown", "-t", "gfm", src_path, "-o", dst_path], None))

            for i in range(1000):
                bucket = f"{i // 100:02d}"
                out_dir = os.path.join(OUT, "text", bucket)
                os.makedirs(out_dir, exist_ok=True)
                _, src = make_markdown_source(i, "txt", paras)
                src_path = os.path.join(tmp, f"txt-{i:04d}.md")
                with open(src_path, "w", encoding="utf-8") as fp:
                    fp.write(src)
                dst_path = os.path.join(out_dir, f"doc-{i:04d}.txt")
                tasks.append((["pandoc", "-f", "markdown", "-t", "plain", src_path, "-o", dst_path], None))

            for i in range(1000):
                bucket = f"{i // 100:02d}"
                out_dir = os.path.join(OUT, "html", bucket)
                os.makedirs(out_dir, exist_ok=True)
                title, src = make_markdown_source(i, "html", paras)
                src_path = os.path.join(tmp, f"html-{i:04d}.md")
                with open(src_path, "w", encoding="utf-8") as fp:
                    fp.write(src)
                dst_path = os.path.join(out_dir, f"doc-{i:04d}.html")
                tasks.append((
                    ["pandoc", "-f", "markdown", "-t", "html5", "--standalone", "--metadata", f"title={title}", src_path, "-o", dst_path],
                    None,
                ))

            for i in range(500):
                bucket = f"{i // 100:02d}"
                out_dir = os.path.join(OUT, "docx", bucket)
                os.makedirs(out_dir, exist_ok=True)
                title, src = make_markdown_source(i, "docx", paras)
                src_path = os.path.join(tmp, f"docx-{i:04d}.md")
                with open(src_path, "w", encoding="utf-8") as fp:
                    fp.write(src)
                dst_path = os.path.join(out_dir, f"doc-{i:04d}.docx")
                tasks.append((
                    ["pandoc", "-f", "markdown", "-t", "docx", "--metadata", f"title={title}", src_path, "-o", dst_path],
                    (dst_path, False),
                ))

            for i in range(500):
                bucket = f"{i // 100:02d}"
                out_dir = os.path.join(OUT, "odt", bucket)
                os.makedirs(out_dir, exist_ok=True)
                title, src = make_markdown_source(i, "odt", paras)
                src_path = os.path.join(tmp, f"odt-{i:04d}.md")
                with open(src_path, "w", encoding="utf-8") as fp:
                    fp.write(src)
                dst_path = os.path.join(out_dir, f"doc-{i:04d}.odt")
                tasks.append((
                    ["pandoc", "-f", "markdown", "-t", "odt", "--metadata", f"title={title}", src_path, "-o", dst_path],
                    (dst_path, True),
                ))

            for i in range(1500):
                bucket = f"{i // 100:02d}"
                out_dir = os.path.join(OUT, "pdf", bucket)
                os.makedirs(out_dir, exist_ok=True)
                src = make_typst_source(i, paras)
                src_path = os.path.join(tmp, f"pdf-{i:04d}.typ")
                with open(src_path, "w", encoding="utf-8") as fp:
                    fp.write(src)
                dst_path = os.path.join(out_dir, f"doc-{i:04d}.pdf")
                tasks.append((
                    [
                        "typst",
                        "compile",
                        "--ignore-system-fonts",
                        "--creation-timestamp",
                        "315532800",
                        "-j",
                        "1",
                        src_path,
                        dst_path,
                    ],
                    None,
                ))

            def run_task(item):
                cmd, zip_fix = item
                subprocess.run(cmd, check=True)
                if zip_fix is not None:
                    normalize_zip_file(zip_fix[0], is_odf=zip_fix[1])

            with ThreadPoolExecutor(max_workers=WORKERS) as pool:
                for fut in [pool.submit(run_task, t) for t in tasks]:
                    fut.result()

        # 3. Spreadsheets: 200 XLSX, 150 ODS, 150 CSV (500 total = 5%)
        n_paras = len(paras)
        fixed_dt_obj = datetime.datetime(1980, 1, 1, 0, 0, 0)

        for i in range(200):
            bucket = f"{i // 100:02d}"
            out_dir = os.path.join(OUT, "xlsx", bucket)
            os.makedirs(out_dir, exist_ok=True)
            wb = openpyxl.Workbook()
            wb.properties.created = fixed_dt_obj
            wb.properties.modified = fixed_dt_obj
            ws = wb.active
            ws.title = "Records"
            ws.append(["row_id", "book", "token", "excerpt"])
            for r in range(1, 26):
                book_title, text = paras[(i * 13 + r) % n_paras]
                ws.append([r, book_title, f"corpus-xlsx-{i:04d}-r{r}", text[:160]])
            buf = io.BytesIO()
            wb.save(buf)
            dst_path = os.path.join(out_dir, f"sheet-{i:04d}.xlsx")
            with open(dst_path, "wb") as fp:
                fp.write(normalize_zip_bytes(buf.getvalue(), is_odf=False))

        for i in range(150):
            bucket = f"{i // 100:02d}"
            out_dir = os.path.join(OUT, "ods", bucket)
            os.makedirs(out_dir, exist_ok=True)
            doc = OpenDocumentSpreadsheet()
            table = Table(name="Records")
            header_row = TableRow()
            for col in ("row_id", "book", "token", "excerpt"):
                tc = TableCell()
                tc.addElement(P(text=col))
                header_row.addElement(tc)
            table.addElement(header_row)
            for r in range(1, 26):
                book_title, text = paras[(i * 17 + r) % n_paras]
                tr = TableRow()
                for val in (str(r), book_title, f"corpus-ods-{i:04d}-r{r}", text[:160]):
                    tc = TableCell()
                    tc.addElement(P(text=val))
                    tr.addElement(tc)
                table.addElement(tr)
            doc.spreadsheet.addElement(table)
            buf = io.BytesIO()
            doc.save(buf)
            dst_path = os.path.join(out_dir, f"sheet-{i:04d}.ods")
            with open(dst_path, "wb") as fp:
                fp.write(normalize_zip_bytes(buf.getvalue(), is_odf=True))

        for i in range(150):
            bucket = f"{i // 100:02d}"
            out_dir = os.path.join(OUT, "csv", bucket)
            os.makedirs(out_dir, exist_ok=True)
            dst_path = os.path.join(out_dir, f"sheet-{i:04d}.csv")
            with open(dst_path, "w", encoding="utf-8", newline="") as fp:
                writer = csv.writer(fp, lineterminator="\n")
                writer.writerow(["row_id", "book", "token", "excerpt"])
                for r in range(1, 26):
                    book_title, text = paras[(i * 19 + r) % n_paras]
                    writer.writerow([r, book_title, f"corpus-csv-{i:04d}-r{r}", text[:160]])

        # 4. Collect all 10,000 files, verify counts, compute BLAKE3 and write MANIFEST
        all_files: list[str] = []
        for root, dirs, files in os.walk(OUT):
            dirs.sort()
            for f in sorted(files):
                if f == "MANIFEST":
                    continue
                rel = os.path.relpath(os.path.join(root, f), OUT)
                all_files.append(rel)
        all_files.sort()

        if len(all_files) != 10000:
            raise RuntimeError(f"Expected 10000 files in corpus, got {len(all_files)}")

        total_bytes = 0
        total_pdf_pages = 0
        counts = {
            "code": 0,
            "markdown": 0,
            "text": 0,
            "html": 0,
            "pdf": 0,
            "docx": 0,
            "odt": 0,
            "xlsx": 0,
            "ods": 0,
            "csv": 0,
        }

        page_re = re.compile(rb"/Type\s*/Page\b")
        for rel in all_files:
            full = os.path.join(OUT, rel)
            top = rel.split("/", 1)[0]
            counts[top] += 1
            st = os.stat(full)
            total_bytes += st.st_size
            if top == "pdf":
                with open(full, "rb") as fp:
                    pdf_bytes = fp.read()
                pages = len(page_re.findall(pdf_bytes))
                if pages != 10:
                    raise RuntimeError(f"Expected 10 pages in {rel}, got {pages}")
                total_pdf_pages += pages

        hashes: dict[str, str] = {}
        batch_size = 500
        for start in range(0, len(all_files), batch_size):
            batch = all_files[start : start + batch_size]
            res = subprocess.run(
                ["b3sum", "--no-names"] + [os.path.join(OUT, r) for r in batch],
                check=True,
                capture_output=True,
                text=True,
            )
            lines = [line.strip() for line in res.stdout.splitlines() if line.strip()]
            if len(lines) != len(batch):
                raise RuntimeError("b3sum output line count mismatch")
            for rel, digest in zip(batch, lines):
                hashes[rel] = digest

        manifest_path = os.path.join(OUT, "MANIFEST")
        with open(manifest_path, "w", encoding="utf-8") as fp:
            fp.write("# nixarchy-llm-index benchmark corpus manifest\n")
            fp.write(f"total_files: {len(all_files)}\n")
            fp.write(f"total_bytes: {total_bytes}\n")
            fp.write(f"total_pdf_pages: {total_pdf_pages}\n")
            fp.write(f"count_code: {counts['code']}\n")
            fp.write(f"count_markdown: {counts['markdown']}\n")
            fp.write(f"count_text: {counts['text']}\n")
            fp.write(f"count_html: {counts['html']}\n")
            fp.write(f"count_pdf: {counts['pdf']}\n")
            fp.write(f"count_docx: {counts['docx']}\n")
            fp.write(f"count_odt: {counts['odt']}\n")
            fp.write(f"count_xlsx: {counts['xlsx']}\n")
            fp.write(f"count_ods: {counts['ods']}\n")
            fp.write(f"count_csv: {counts['csv']}\n")
            fp.write("\n")
            for rel in all_files:
                fp.write(f"{hashes[rel]}  {rel}\n")


    if __name__ == "__main__":
        main()
  '';
in
pkgs.stdenvNoCC.mkDerivation {
  pname = "llm-index-bench-corpus";
  version = "0.1.0";

  dontUnpack = true;

  nativeBuildInputs = [
    pythonEnv
    pkgs.pandoc
    pkgs.typst
    pkgs.b3sum
  ];

  NIXPKGS_SRC = "${nixpkgs}";
  BOOKS_JSON = "${booksJson}";

  buildPhase = ''
    runHook preBuild
    python3 ${builderScript}
    runHook postBuild
  '';

  dontInstall = true;
}
