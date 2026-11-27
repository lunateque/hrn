//! End-to-end tests of the rich `read_file` formats through `read_sync`.

use crate::fs_read::{FileReadTracker, read_sync};
use crate::testutil::{TempDir, test_ctx};
use flate2::Compression;
use flate2::write::{DeflateEncoder, GzEncoder};
use std::fmt::Write as _;
use std::io::Write as _;

pub(super) fn read(dir: &TempDir, raw: &str) -> String {
    read_sync(
        &FileReadTracker::new(),
        &test_ctx(dir.path().to_path_buf()),
        raw,
    )
    .unwrap()
    .content
}

fn read_err(dir: &TempDir, raw: &str) -> String {
    read_sync(
        &FileReadTracker::new(),
        &test_ctx(dir.path().to_path_buf()),
        raw,
    )
    .unwrap_err()
    .to_string()
}

pub(super) fn put(dir: &TempDir, rel: &str, bytes: &[u8]) {
    let path = dir.path().join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn le(out: &mut Vec<u8>, value: u64, bytes: usize) {
    out.extend_from_slice(value.to_le_bytes().get(..bytes).unwrap());
}

/// A zip with one entry per `(name, bytes)`; alternate entries are deflated.
pub(super) fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (i, (name, data)) in entries.iter().enumerate() {
        let deflate = i % 2 == 1;
        let stored: Vec<u8> = if deflate {
            let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
            enc.write_all(data).unwrap();
            enc.finish().unwrap()
        } else {
            data.to_vec()
        };
        let method = if deflate { 8 } else { 0 };
        let offset = out.len() as u64;
        out.extend_from_slice(b"PK\x03\x04");
        for (v, n) in [(20, 2), (0, 2), (method, 2), (0, 4), (0, 4)] {
            le(&mut out, v, n);
        }
        le(&mut out, stored.len() as u64, 4);
        le(&mut out, data.len() as u64, 4);
        le(&mut out, name.len() as u64, 2);
        le(&mut out, 0, 2);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&stored);

        central.extend_from_slice(b"PK\x01\x02");
        for (v, n) in [(20, 2), (20, 2), (0, 2), (method, 2), (0, 4), (0, 4)] {
            le(&mut central, v, n);
        }
        le(&mut central, stored.len() as u64, 4);
        le(&mut central, data.len() as u64, 4);
        le(&mut central, name.len() as u64, 2);
        for (v, n) in [(0, 2), (0, 2), (0, 2), (0, 2), (0, 4)] {
            le(&mut central, v, n);
        }
        le(&mut central, offset, 4);
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u64;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06");
    le(&mut out, 0, 4);
    le(&mut out, entries.len() as u64, 2);
    le(&mut out, entries.len() as u64, 2);
    le(&mut out, central.len() as u64, 4);
    le(&mut out, cd_offset, 4);
    le(&mut out, 0, 2);
    out
}

pub(super) fn tar_bytes(entries: &[(&str, &[u8], u8)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data, flag) in entries {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..107].copy_from_slice(b"0000644");
        let size = format!("{:011o}", data.len());
        header[124..135].copy_from_slice(size.as_bytes());
        header[156] = *flag;
        header[257..262].copy_from_slice(b"ustar");
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.resize(out.len() + 1024, 0);
    out
}

#[test]
fn zip_lists_reads_entries_and_applies_selectors() {
    let dir = TempDir::new("fmt-zip");
    let zip = zip_bytes(&[
        ("src/a.txt", b"one\ntwo\nthree\n"),
        ("src/deep/b.txt", b"deflated body\nsecond line\n"),
        ("top.bin", &[0, 1, 2, 3]),
        ("data.json", br#"{"items":[{"n":1},{"n":2}]}"#),
    ]);
    put(&dir, "a.zip", &zip);
    let root = read(&dir, "a.zip");
    assert_eq!(root, "src/\ndata.json (27 B)\ntop.bin (4 B)");
    assert_eq!(read(&dir, "a.zip:src"), "deep/\na.txt (14 B)");
    let entry = read(&dir, "a.zip:src/a.txt:2");
    assert!(entry.contains("2:two"), "{entry}");
    // Deflated entry plus a raw selector.
    assert_eq!(read(&dir, "a.zip:src/deep/b.txt:raw:2"), "second line");
    assert!(read(&dir, "a.zip:top.bin").starts_with("[Cannot read binary archive entry 'top.bin'"));
    let q = read(&dir, "a.zip:data.json?q=.items[].n");
    assert!(q.contains("1:1") && q.contains("2:2"), "{q}");
    assert!(read_err(&dir, "a.zip:../x").contains(".."));
    assert!(read_err(&dir, "a.zip:nope.txt").contains("no entry"));
}

#[test]
fn tar_and_tar_gz_list_and_read() {
    let dir = TempDir::new("fmt-tar");
    let tar = tar_bytes(&[
        ("pkg/", b"", b'5'),
        ("pkg/readme.txt", b"hello tar\n", b'0'),
        ("pkg/big.txt", &vec![b'x'; 700], b'0'),
    ]);
    put(&dir, "a.tar", &tar);
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&tar).unwrap();
    put(&dir, "a.tar.gz", &gz.finish().unwrap());
    for archive in ["a.tar", "a.tar.gz"] {
        assert_eq!(read(&dir, archive), "pkg/", "{archive}");
        assert_eq!(
            read(&dir, &format!("{archive}:pkg")),
            "big.txt (700 B)\nreadme.txt (10 B)"
        );
        let text = read(&dir, &format!("{archive}:pkg/readme.txt"));
        assert!(text.contains("1:hello tar"), "{text}");
    }
}

#[test]
fn sqlite_lists_describes_pages_and_stays_read_only() {
    let dir = TempDir::new("fmt-sql");
    {
        let conn = rusqlite::Connection::open(dir.path().join("t.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, note BLOB);\
             CREATE TABLE empty_t(x);",
        )
        .unwrap();
        for i in 1..=30 {
            conn.execute(
                "INSERT INTO users(name, note) VALUES (?1, x'00ff')",
                [format!("user{i}")],
            )
            .unwrap();
        }
    }
    let list = read(&dir, "t.sqlite");
    assert!(
        list.contains("users (30 rows)") && list.contains("empty_t (0 rows)"),
        "{list}"
    );
    let schema = read(&dir, "t.sqlite:users");
    assert!(schema.contains("CREATE TABLE users"), "{schema}");
    assert!(
        schema.contains("| user1") && !schema.contains("user6 "),
        "{schema}"
    );
    assert!(schema.contains("<blob 2 bytes>"), "{schema}");
    let q = read(
        &dir,
        "t.sqlite?q=SELECT name FROM users WHERE id > 27 ORDER BY id",
    );
    assert!(q.contains("user30") && q.contains("3 row(s)"), "{q}");
    let page = read(&dir, "t.sqlite?q=SELECT id FROM users&limit=5&offset=10");
    assert!(
        page.contains("| 11") && page.contains("| 15") && !page.contains("| 16"),
        "{page}"
    );
    assert!(page.contains("&offset=15"), "{page}");
    let rows = read(&dir, "t.sqlite:users?limit=2&order=id:desc");
    assert!(
        rows.contains("| 30") && rows.contains("| 29") && !rows.contains("| 28"),
        "{rows}"
    );
    assert!(read_err(&dir, "t.sqlite?q=DELETE FROM users").contains("read-only"));
    assert!(read_err(&dir, "t.sqlite:nope").contains("no table"));
    assert!(read(&dir, "t.sqlite?q=SELECT count(*) AS n FROM users").contains("| 30"));
}

fn docx(body: &str) -> Vec<u8> {
    let xml = format!(
        "<?xml version=\"1.0\"?><w:document xmlns:w=\"x\"><w:body>{body}</w:body></w:document>"
    );
    zip_bytes(&[("word/document.xml", xml.as_bytes())])
}

#[test]
fn docx_keeps_headings_paragraphs_and_tables() {
    let dir = TempDir::new("fmt-docx");
    put(
        &dir,
        "d.docx",
        &docx(
            "<w:p><w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr><w:r><w:t>Title &amp; more</w:t></w:r></w:p>\
             <w:p><w:r><w:t xml:space=\"preserve\">Hello </w:t></w:r><w:r><w:t>world</w:t></w:r></w:p>\
             <w:tbl><w:tr><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p></w:tc>\
             <w:tc><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc></w:tr></w:tbl>",
        ),
    );
    let text = read(&dir, "d.docx");
    assert!(
        text.contains("1:# Title & more\n2:Hello world\n3:a | b"),
        "{text}"
    );
}

#[test]
fn xlsx_and_pptx_and_epub_render_text() {
    let dir = TempDir::new("fmt-office");
    let sheet = "<worksheet><sheetData><row r=\"1\"><c r=\"A1\" t=\"s\"><v>0</v></c>\
                 <c r=\"C1\"><v>42</v></c></row><row r=\"2\"><c r=\"B2\" t=\"inlineStr\"><is><t>x</t></is></c></row>\
                 </sheetData></worksheet>";
    put(
        &dir,
        "s.xlsx",
        &zip_bytes(&[
            ("xl/workbook.xml", b"<workbook><sheets><sheet name=\"Data\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>"),
            ("xl/_rels/workbook.xml.rels", b"<Relationships><Relationship Id=\"rId1\" Target=\"worksheets/sheet1.xml\"/></Relationships>"),
            ("xl/sharedStrings.xml", b"<sst><si><t>Name</t></si></sst>"),
            ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
        ]),
    );
    let xlsx = read(&dir, "s.xlsx");
    assert!(
        xlsx.contains("## Sheet: Data")
            && xlsx.contains("1: Name |  | 42")
            && xlsx.contains("2:  | x"),
        "{xlsx}"
    );

    put(
        &dir,
        "p.pptx",
        &zip_bytes(&[
            (
                "ppt/slides/slide2.xml",
                b"<p:sld><a:p><a:r><a:t>Second</a:t></a:r></a:p></p:sld>",
            ),
            (
                "ppt/slides/slide1.xml",
                b"<p:sld><a:p><a:r><a:t>First</a:t></a:r></a:p></p:sld>",
            ),
            (
                "ppt/notesSlides/notesSlide1.xml",
                b"<p:notes><a:p><a:r><a:t>say hi</a:t></a:r></a:p></p:notes>",
            ),
        ]),
    );
    let pptx = read(&dir, "p.pptx:raw");
    assert!(
        pptx.find("First").unwrap() < pptx.find("Second").unwrap(),
        "{pptx}"
    );
    assert!(pptx.contains("Notes:\nsay hi"), "{pptx}");

    put(
        &dir,
        "b.epub",
        &zip_bytes(&[
            ("META-INF/container.xml", b"<container><rootfiles><rootfile full-path=\"OEBPS/content.opf\"/></rootfiles></container>"),
            ("OEBPS/content.opf", b"<package><manifest><item id=\"c1\" href=\"ch1.xhtml\"/><item id=\"c2\" href=\"text/ch2.xhtml\"/></manifest><spine><itemref idref=\"c2\"/><itemref idref=\"c1\"/></spine></package>"),
            ("OEBPS/ch1.xhtml", b"<html><body><h1>One</h1><p>first &amp; text</p></body></html>"),
            ("OEBPS/text/ch2.xhtml", b"<html><body><p>Two</p></body></html>"),
        ]),
    );
    let epub = read(&dir, "b.epub:raw");
    assert!(
        epub.contains("Two\n\n---\n\nOne") && epub.contains("first & text"),
        "{epub}"
    );
}

#[test]
fn notebook_numbers_cells_and_raw_stays_json() {
    let dir = TempDir::new("fmt-nb");
    let nb = r##"{"cells":[{"cell_type":"markdown","source":["# Hi\n","text"]},{"cell_type":"code","source":"print(1)\n","outputs":[]}]}"##;
    put(&dir, "n.ipynb", nb.as_bytes());
    let text = read(&dir, "n.ipynb:raw:1-2");
    assert!(text.starts_with("{\"cells\""), "{text}");
    let cells = read(&dir, "n.ipynb");
    assert!(
        cells.contains(
            "1:# %% [markdown] cell:0\n2:# Hi\n3:text\n4:\n5:# %% [code] cell:1\n6:print(1)"
        ),
        "{cells}"
    );
}

#[test]
fn json_queries_cover_the_jq_subset_and_jsonl() {
    let dir = TempDir::new("fmt-json");
    put(
        &dir,
        "d.json",
        br#"{"name":"x","tags":["a","b","c"],"items":[{"id":1,"ok":true},{"id":2,"ok":false},{"id":3,"ok":true}],"nested":{"k":"v"}}"#,
    );
    let run = |q: &str| read(&dir, &format!("d.json?q={q}&compact=true"));
    assert!(run(".name").contains("1:\"x\""));
    assert!(run(".tags[1]").contains("\"b\""));
    assert!(run(".tags[-1]").contains("\"c\""));
    assert!(run(".tags|length").contains("1:3"));
    assert!(run(".nested|keys").contains("[\"k\"]"));
    assert!(run(".items[]|select(.id>=2 and .ok==true)|.id").contains("1:3"));
    assert!(run(".items|map(.id)").contains("[1,2,3]"));
    assert!(run(".tags[1:]").contains("[\"b\",\"c\"]"));
    assert!(run(".missing.deeper").contains("null"));
    assert!(run(".name.x?").contains("(no results)"));
    assert!(read_err(&dir, "d.json?q=.name.x").contains("cannot index"));
    assert!(read_err(&dir, "d.json?q=.items[] | nope").contains("unsupported function"));
    assert!(read(&dir, "d.json?q=.name&raw=true").contains("1:x"));
    let paged = read(&dir, "d.json?q=.tags&limit=2&compact=true");
    assert!(
        paged.contains("[\"a\",\"b\"]") && paged.contains("&offset=2"),
        "{paged}"
    );

    put(&dir, "l.jsonl", b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n");
    let lines = read(&dir, "l.jsonl?q=select(.a>1)|.a");
    assert!(lines.contains("1:2") && lines.contains("2:3"), "{lines}");
}

#[test]
fn pdf_text_is_extracted_per_page() {
    let dir = TempDir::new("fmt-pdf");
    let content = "BT /F1 12 Tf 72 700 Td (Hello PDF world) Tj ET";
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
         /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_owned(),
        format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, body) in objs.iter().enumerate() {
        offsets.push(pdf.len());
        let _ = write!(pdf, "{} 0 obj\n{body}\nendobj\n", i + 1);
    }
    let xref = pdf.len();
    let _ = write!(pdf, "xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1);
    for o in offsets {
        let _ = writeln!(pdf, "{o:010} 00000 n ");
    }
    let _ = write!(
        pdf,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objs.len() + 1
    );
    put(&dir, "d.pdf", pdf.as_bytes());
    let text = read(&dir, "d.pdf");
    assert!(
        text.contains("## Page 1") && text.contains("Hello PDF world"),
        "{text}"
    );
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

#[test]
fn pdf_object_streams_cid_fonts_and_word_gaps() {
    let dir = TempDir::new("fmt-pdf2");
    // Objects 1-4 live in a compressed object stream; the content stream and the CMap are
    // Flate streams. Type0 font with a ToUnicode map: codes 1 and 2 are H and i, 3 is `!`.
    let inner = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /Contents 6 0 R /Resources << /Font << /F1 4 0 R >> >> >>",
        "<< /Type /Font /Subtype /Type0 /BaseFont /X /Encoding /Identity-H /ToUnicode 7 0 R \
         /DescendantFonts [<< /Type /Font /Subtype /CIDFontType2 /DW 500 >>] >>",
    ];
    let mut header = String::new();
    let mut body = String::new();
    for (i, o) in inner.iter().enumerate() {
        let _ = write!(header, "{} {} ", i + 1, body.len());
        body.push_str(o);
        body.push('\n');
    }
    let objstm = deflate(format!("{header}{body}").as_bytes());
    let content = deflate(
        b"BT /F1 10 Tf 72 700 Td <00010002> Tj 100 0 Td <0003> Tj 0 -14 Td [<0001> -400 <0002>] TJ ET",
    );
    let cmap = deflate(
        b"1 begincodespacerange <0000> <FFFF> endcodespacerange\n\
          2 beginbfchar <0001> <0048> <0002> <0069> endbfchar\n\
          1 beginbfrange <0003> <0004> <0021> endbfrange",
    );
    let mut pdf: Vec<u8> = b"%PDF-1.5\n".to_vec();
    let mut push_stream = |id: u32, dict: &str, data: &[u8]| {
        let head = format!("{id} 0 obj\n<< {dict} /Length {} >>\nstream\n", data.len());
        pdf.extend_from_slice(head.as_bytes());
        pdf.extend_from_slice(data);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
    };
    push_stream(
        5,
        &format!(
            "/Type /ObjStm /N 4 /First {} /Filter /FlateDecode",
            header.len()
        ),
        &objstm,
    );
    push_stream(6, "/Filter /FlateDecode", &content);
    push_stream(7, "/Filter /FlateDecode", &cmap);
    pdf.extend_from_slice(b"trailer\n<< /Root 1 0 R >>\n%%EOF\n");
    put(&dir, "c.pdf", &pdf);
    let text = read(&dir, "c.pdf:raw");
    assert_eq!(text.trim_end(), "## Page 1\nHi !\nH i");
}

#[test]
fn plain_files_and_unrelated_colons_fall_through() {
    let dir = TempDir::new("fmt-none");
    put(&dir, "notes.txt", b"a\nb\n");
    put(&dir, "fake.zip", b"not a zip at all");
    assert!(read(&dir, "notes.txt:2").contains("2:b"));
    assert!(read(&dir, "fake.zip").contains("1:not a zip at all"));
}
