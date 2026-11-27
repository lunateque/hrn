Use `read` for static web; browser only if needed.

Put line ranges in `path`, never a separate range/offset/limit parameter:
- `{"path":"notes.txt:450-455"}` reads lines 450-455; `{"path":"notes.txt:-5"}` reads the last 5.
- Include the selector on every ranged call; repeating a bare path does not advance through the file.

Path suffixes:
- :50 line 50; :50- from line 50 on; :50-200 inclusive; :50+150 counts lines; :-60 last 60.
- Commas join ranges (:5-16,960-973) or individual lines (:19,59).
- A non-raw range also shows 1 line before and 3 after a cut block; :raw stays exact.
- :raw verbatim without line numbers; combine (:2-4:raw) for exactly those lines.

Sources:
- A bare file shows 300 numbered lines; the footer names the selector that continues a cut read.
- Directory: complete root; child listings cap at 12 (`… N more`), read child; page via :N-M/:-N.
- URLs (`https://…`, `www.…`): reader text; :raw original HTML.
- PDF/DOCX/PPTX/XLSX/EPUB: extracted text; notebooks: editable cells; images: decoded inline.
- Also, with selectors in help://read/sources (read it before first use):
  SQLite, archives (ZIP, TAR and zip-family forms), JSON/JSONL by jq filter,
  executable metadata, SVG, merge conflicts.
