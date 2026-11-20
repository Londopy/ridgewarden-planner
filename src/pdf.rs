//! A small PDF 1.4 writer: US Letter pages with Helvetica text, lines and dots. Enough
//! for a report that prints and attaches to a grant application; nothing more.

use std::fmt::Write as _;

pub const W: f64 = 612.0;
pub const H: f64 = 792.0;
pub const MARGIN: f64 = 54.0;

/// Helvetica glyph widths (per 1000 em) for ASCII 32 to 126, from the standard AFM.
const WIDTHS: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667,
    611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500,
    222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];

/// Width of `s` in points at `size`; bold is drawn a little wider, so allow for it.
pub fn width(s: &str, size: f64, bold: bool) -> f64 {
    let w: u32 = s
        .chars()
        .map(|c| {
            WIDTHS
                .get((c as usize).wrapping_sub(32))
                .copied()
                .unwrap_or(556) as u32
        })
        .sum();
    w as f64 * size / 1000.0 * if bold { 1.06 } else { 1.0 }
}

/// Text as a PDF string: printable ASCII only, with the delimiters escaped.
fn literal(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('(');
    for c in s.chars() {
        match c {
            '(' | ')' | '\\' => {
                o.push('\\');
                o.push(c);
            }
            ' '..='~' => o.push(c),
            _ => o.push('?'),
        }
    }
    o.push(')');
    o
}

/// One page's drawing commands. Coordinates are points from the bottom left.
#[derive(Default)]
pub struct Page {
    ops: String,
}

impl Page {
    pub fn text(&mut self, x: f64, y: f64, size: f64, bold: bool, s: &str) {
        let _ = writeln!(
            self.ops,
            "BT /{} {size:.1} Tf {x:.2} {y:.2} Td {} Tj ET",
            if bold { "F2" } else { "F1" },
            literal(s)
        );
    }

    pub fn gray(&mut self, g: f64) {
        let _ = writeln!(self.ops, "{g:.2} G {g:.2} g");
    }

    pub fn line(&mut self, (x1, y1): (f64, f64), (x2, y2): (f64, f64), width: f64, dashed: bool) {
        let dash = if dashed { "[3 3] 0 d" } else { "[] 0 d" };
        let _ = writeln!(
            self.ops,
            "{dash} {width:.2} w {x1:.2} {y1:.2} m {x2:.2} {y2:.2} l S"
        );
    }

    /// A filled dot, as four Bezier quarter circles.
    pub fn dot(&mut self, (x, y): (f64, f64), r: f64) {
        let k = 0.5523 * r;
        let _ = writeln!(
            self.ops,
            "{:.2} {y:.2} m {:.2} {:.2} {:.2} {:.2} {x:.2} {:.2} c {:.2} {:.2} {:.2} {:.2} {:.2} {y:.2} c \
             {:.2} {:.2} {:.2} {:.2} {x:.2} {:.2} c {:.2} {:.2} {:.2} {:.2} {:.2} {y:.2} c f",
            x + r,
            x + r,
            y + k,
            x + k,
            y + r,
            y + r,
            x - k,
            y + r,
            x - r,
            y + k,
            x - r,
            x - r,
            y - k,
            x - k,
            y - r,
            y - r,
            x + k,
            y - r,
            x + r,
            y - k,
            x + r
        );
    }

    pub fn rect(&mut self, (x, y): (f64, f64), (w, h): (f64, f64)) {
        let _ = writeln!(self.ops, "[] 0 d 0.5 w {x:.2} {y:.2} {w:.2} {h:.2} re S");
    }
}

/// Lays out flowing text down pages, starting a new page when one fills.
pub struct Writer {
    pub pages: Vec<Page>,
    pub y: f64,
}

impl Default for Writer {
    fn default() -> Self {
        Self {
            pages: vec![Page::default()],
            y: H - MARGIN,
        }
    }
}

impl Writer {
    pub fn page(&mut self) -> &mut Page {
        self.pages.last_mut().expect("a writer always has a page")
    }

    /// Make room for `h` points, on a new page if this one is too full.
    pub fn room(&mut self, h: f64) {
        if self.y - h < MARGIN {
            self.pages.push(Page::default());
            self.y = H - MARGIN;
        }
    }

    /// A paragraph wrapped to the margins.
    pub fn para(&mut self, s: &str, size: f64, bold: bool) {
        let lead = size * 1.3;
        let max = W - 2.0 * MARGIN;
        let mut line = String::new();
        for word in s.split_whitespace() {
            let next = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if width(&next, size, bold) > max && !line.is_empty() {
                self.room(lead);
                let y = self.y - size;
                self.page().text(MARGIN, y, size, bold, &line);
                self.y -= lead;
                line = word.to_string();
            } else {
                line = next;
            }
        }
        if !line.is_empty() {
            self.room(lead);
            let y = self.y - size;
            self.page().text(MARGIN, y, size, bold, &line);
            self.y -= lead;
        }
        self.y -= size * 0.4;
    }

    pub fn heading(&mut self, s: &str) {
        self.room(40.0);
        self.y -= 6.0;
        self.para(s, 13.0, true);
    }

    /// The finished file.
    pub fn finish(self, title: &str) -> Vec<u8> {
        document(title, &self.pages)
    }
}

/// Serialize pages into a PDF file with a correct cross-reference table.
pub fn document(title: &str, pages: &[Page]) -> Vec<u8> {
    let n = pages.len();
    // 1 catalog, 2 page tree, 3 and 4 fonts, 5 info, then a page and its content each
    let mut objs: Vec<String> =
        vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {n} >>",
            (0..n)
                .map(|i| format!("{} 0 R", 6 + 2 * i))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>"
            .into(),
        format!("<< /Title {} /Producer (ridgewarden-planner) >>", literal(title)),
    ];
    for (i, p) in pages.iter().enumerate() {
        objs.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {W} {H}] \
             /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {} 0 R >>",
            7 + 2 * i
        ));
        objs.push(format!(
            "<< /Length {} >>\nstream\n{}endstream",
            p.ops.len(),
            p.ops
        ));
    }
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::with_capacity(objs.len());
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        let _ = write!(out, "{} 0 obj\n{o}\nendobj\n", i + 1);
    }
    let xref = out.len();
    let _ = write!(out, "xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1);
    for off in offsets {
        let _ = writeln!(out, "{off:010} 00000 n ");
    }
    let _ = write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R /Info 5 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objs.len() + 1
    );
    out.into_bytes()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Check a file the way a reader does: header, the xref offsets each land on their
    /// object, startxref lands on the xref, and the page tree counts the pages.
    pub fn check(pdf: &[u8]) -> String {
        let s = String::from_utf8(pdf.to_vec()).expect("ASCII only");
        assert!(s.starts_with("%PDF-1.4\n") && s.ends_with("%%EOF\n"));
        let start: usize = s
            .rsplit("startxref\n")
            .next()
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            s[start..].starts_with("xref\n0 "),
            "startxref points at the xref"
        );
        let mut lines = s[start..].lines().skip(1);
        let count: usize = lines
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        lines.next();
        for i in 1..count {
            let off: usize = lines.next().unwrap()[..10].parse().unwrap();
            assert!(
                s[off..].starts_with(&format!("{i} 0 obj\n")),
                "xref entry {i} points at its object"
            );
        }
        let mut from = 0;
        while let Some(at) = s[from..].find(">>\nstream\n") {
            let start = from + at + ">>\nstream\n".len();
            let end = start + s[start..].find("endstream").unwrap();
            let len: usize = s[..start]
                .rsplit("/Length ")
                .next()
                .unwrap()
                .split(' ')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(len, end - start, "stream /Length matches");
            from = end;
        }
        s
    }

    #[test]
    fn writes_a_well_formed_file() {
        let mut w = Writer::default();
        w.heading("Mesh (plan)");
        for i in 0..120 {
            w.para(&format!("Line {i} with a backslash \\ and a long tail of words that wraps across the page width more than once"), 10.0, false);
        }
        let mut p = Page::default();
        p.line((10.0, 10.0), (100.0, 100.0), 1.0, true);
        p.dot((50.0, 50.0), 3.0);
        w.pages.push(p);
        let pages = w.pages.len();
        let s = check(&w.finish("Plan \u{e9}"));
        assert!(pages > 3, "long text spills over pages");
        assert!(s.contains(&format!("/Count {pages}")));
        assert!(s.contains("(Mesh \\(plan\\)) Tj"));
        assert!(s.contains("backslash \\\\ and"));
        assert!(s.contains("/Title (Plan ?)"), "non-ASCII is replaced");
    }

    #[test]
    fn wrapping_stays_inside_the_margins() {
        let mut w = Writer::default();
        w.para(&"word ".repeat(400), 10.0, false);
        let ops = &w.pages[0].ops;
        for line in ops.lines().filter(|l| l.contains(" Tj")) {
            let text = &line[line.find('(').unwrap() + 1..line.rfind(')').unwrap()];
            assert!(width(text, 10.0, false) <= W - 2.0 * MARGIN);
        }
    }
}
