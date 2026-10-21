//! HTML outputs (text/html) for rhun, which draws no HTML: as Markdown for a rich item (tables as
//! pipe tables, the most common rich output being a pandas DataFrame) and as plain text with its
//! tables in aligned columns. Scripts, styles and comments go; unknown tags keep their text.

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Markdown,
    Text,
}

/// The HTML as Markdown (views' subset: headings, paragraphs, lists, emphasis, code, links, pipe
/// tables, rules).
pub fn to_markdown(html: &str) -> String {
    convert(html, Mode::Markdown)
}

/// The HTML as plain text: what a browser would show, tables in columns.
pub fn to_text(html: &str) -> String {
    convert(html, Mode::Text)
}

enum Tok {
    Text(String),
    Open(String, Vec<(String, String)>),
    Close(String),
}

fn tokens(html: &str) -> Vec<Tok> {
    let b = html.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut text = String::new();
    while i < b.len() {
        if b[i] == b'<' {
            if html[i..].starts_with("<!--") {
                i = html[i..].find("-->").map(|e| i + e + 3).unwrap_or(b.len());
                continue;
            }
            let name_start = i + 1 + usize::from(b.get(i + 1) == Some(&b'/'));
            if b.get(name_start).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'!' || *c == b'?') {
                let end = tag_end(b, i);
                if !text.is_empty() {
                    out.push(Tok::Text(decode(&std::mem::take(&mut text))));
                }
                let inner = &html[i + 1..end.saturating_sub(1).max(i + 1)];
                i = end;
                if let Some(name) = inner.strip_prefix('/') {
                    out.push(Tok::Close(name.trim().to_ascii_lowercase()));
                    continue;
                }
                if inner.starts_with('!') || inner.starts_with('?') {
                    continue;
                }
                let inner = inner.trim_end_matches('/');
                let n = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
                let name = inner[..n].to_ascii_lowercase();
                let attrs = attributes(&inner[n..]);
                if name == "script" || name == "style" || name == "head" {
                    let close = format!("</{name}");
                    let lower = html[i..].to_ascii_lowercase();
                    i = lower.find(&close).map(|e| i + e).unwrap_or(b.len());
                    i = html[i..].find('>').map(|e| i + e + 1).unwrap_or(b.len());
                    continue;
                }
                out.push(Tok::Open(name, attrs));
                continue;
            }
        }
        let c = html[i..].chars().next().unwrap();
        text.push(c);
        i += c.len_utf8();
    }
    if !text.is_empty() {
        out.push(Tok::Text(decode(&text)));
    }
    out
}

/// Where the tag starting at i ends (after its '>'), quotes respected.
fn tag_end(b: &[u8], mut i: usize) -> usize {
    let mut quote = 0u8;
    while i < b.len() {
        let c = b[i];
        i += 1;
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'>' {
            return i;
        }
    }
    b.len()
}

fn attributes(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let ns = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' {
            i += 1;
        }
        if ns == i {
            i += 1;
            continue;
        }
        let name = s[ns..i].to_ascii_lowercase();
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let vs = i + 1;
                i = vs;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                value = decode(&s[vs..i.min(b.len())]);
                i += 1;
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = decode(&s[vs..i]);
            }
        }
        out.push((name, value));
    }
    out
}

fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::new();
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let end = rest.as_bytes().iter().take(12).position(|&c| c == b';');
        let ent = end.map(|e| &rest[1..e]);
        let ch = ent.and_then(|e| match e {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "times" => Some('\u{d7}'),
            "hellip" => Some('\u{2026}'),
            "mdash" => Some('\u{2014}'),
            "ndash" => Some('\u{2013}'),
            _ if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            _ if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        });
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end.unwrap() + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

struct Table {
    rows: Vec<Vec<String>>,
    head: usize, // rows that came from <thead> or held only <th>
    row: Option<Vec<String>>,
    row_th: bool,
    in_head: bool,
}

struct Conv {
    mode: Mode,
    out: String,
    line: String, // the inline text of the block being made
    lists: Vec<Option<u32>>, // None bullet, Some(n) ordered at n
    quote: usize,
    pre: usize,
    code: usize,
    links: Vec<String>,
    tables: Vec<Table>,
    cell: Option<String>,
}

fn convert(html: &str, mode: Mode) -> String {
    let mut c = Conv {
        mode,
        out: String::new(),
        line: String::new(),
        lists: Vec::new(),
        quote: 0,
        pre: 0,
        code: 0,
        links: Vec::new(),
        tables: Vec::new(),
        cell: None,
    };
    for t in tokens(html) {
        match t {
            Tok::Text(s) => c.text(&s),
            Tok::Open(n, a) => c.open(&n, &a),
            Tok::Close(n) => c.close(&n),
        }
    }
    while !c.tables.is_empty() {
        c.end_table();
    }
    c.flush();
    let mut s = c.out.trim_end().to_string();
    if !s.is_empty() {
        s.push('\n');
    }
    s
}

impl Conv {
    fn md(&self) -> bool {
        self.mode == Mode::Markdown
    }

    fn sink(&mut self) -> &mut String {
        match self.cell.as_mut() {
            Some(c) => c,
            None => &mut self.line,
        }
    }

    fn push(&mut self, s: &str) {
        self.sink().push_str(s);
    }

    fn text(&mut self, s: &str) {
        if self.pre > 0 && self.cell.is_none() {
            self.line.push_str(s);
            return;
        }
        // whitespace collapses
        let mut t = String::new();
        let mut space = self.sink().ends_with(' ') || self.sink().is_empty();
        for ch in s.chars() {
            if ch.is_whitespace() {
                if !space {
                    t.push(' ');
                    space = true;
                }
            } else {
                if self.md() && self.pre == 0 && self.code == 0 && "*_`[]\\|<#".contains(ch) {
                    t.push('\\');
                }
                t.push(ch);
                space = false;
            }
        }
        self.push(&t);
    }

    /// The block made so far into the output, with its quote and list prefixes.
    fn flush(&mut self) {
        let line = std::mem::take(&mut self.line);
        let body = if self.pre > 0 { line.trim_matches('\n').to_string() } else { line.trim().to_string() };
        if body.is_empty() {
            return;
        }
        let quote = if self.md() { "> ".repeat(self.quote) } else { "  ".repeat(self.quote) };
        let indent = "   ".repeat(self.lists.len().saturating_sub(1));
        for l in body.lines() {
            self.out.push_str(&quote);
            if !self.lists.is_empty() {
                self.out.push_str(&indent);
                match l.strip_prefix('\u{1}') {
                    Some(item) => self.out.push_str(item), // a list item's first line: its marker
                    None => {
                        self.out.push_str("   ");
                        self.out.push_str(l);
                    }
                }
            } else {
                self.out.push_str(l.trim_start_matches('\u{1}'));
            }
            self.out.push('\n');
        }
        if self.lists.is_empty() {
            self.out.push_str(&quote);
            self.out.push('\n');
        }
    }

    fn block(&mut self) {
        if self.cell.is_some() {
            self.push(" ");
        } else {
            self.flush();
        }
    }

    fn open(&mut self, name: &str, attrs: &[(String, String)]) {
        let attr = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
        match name {
            "p" | "div" | "section" | "article" | "header" | "footer" | "figure" | "figcaption" | "dl" | "dt" | "dd"
            | "details" | "summary" | "center" => self.block(),
            "br" => {
                if self.cell.is_some() {
                    self.push(" ");
                } else if self.md() && self.pre == 0 {
                    self.line.push_str("\\\n");
                } else {
                    self.line.push('\n');
                }
            }
            "hr" => {
                self.block();
                if self.cell.is_none() {
                    self.out.push_str(if self.md() { "---\n\n" } else { "----------\n\n" });
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.block();
                if self.md() && self.cell.is_none() {
                    let n = (name.as_bytes()[1] - b'0') as usize;
                    self.line.push_str(&"#".repeat(n));
                    self.line.push(' ');
                }
            }
            "b" | "strong" if self.md() && self.pre == 0 => self.push("**"),
            "i" | "em" if self.md() && self.pre == 0 => self.push("*"),
            "s" | "del" | "strike" if self.md() && self.pre == 0 => self.push("~~"),
            "code" | "kbd" | "samp" | "tt" if self.md() && self.pre == 0 => {
                self.push("`");
                self.code += 1;
            }
            "pre" => {
                self.block();
                self.pre += 1;
            }
            "a" => {
                let href = attr("href");
                self.links.push(href);
                if self.md() && self.pre == 0 && !self.links.last().unwrap().is_empty() {
                    self.push("[");
                }
            }
            "img" => {
                let alt = attr("alt");
                if !alt.is_empty() {
                    self.text(&alt);
                }
            }
            "blockquote" => {
                self.block();
                self.quote += 1;
            }
            "ul" | "ol" => {
                self.block();
                let start = attr("start").parse().unwrap_or(1);
                self.lists.push(if name == "ol" { Some(start) } else { None });
            }
            "li" => {
                self.block();
                if self.cell.is_none() {
                    let marker = match self.lists.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}. ");
                            *n += 1;
                            m
                        }
                        _ => if self.md() { "- " } else { "\u{2022} " }.to_string(),
                    };
                    // the marker sits where the list's indentation starts
                    self.line.push('\u{1}');
                    self.line.push_str(&marker);
                }
            }
            "table" => {
                self.block();
                self.tables.push(Table { rows: Vec::new(), head: 0, row: None, row_th: true, in_head: false });
            }
            "thead" => {
                if let Some(t) = self.tables.last_mut() {
                    t.in_head = true;
                }
            }
            "tbody" | "tfoot" => {
                if let Some(t) = self.tables.last_mut() {
                    t.in_head = false;
                }
            }
            "tr" => {
                self.end_row();
                if let Some(t) = self.tables.last_mut() {
                    t.row = Some(Vec::new());
                    t.row_th = true;
                }
            }
            "th" | "td" => {
                self.end_cell();
                if self.tables.is_empty() {
                    return;
                }
                let t = self.tables.last_mut().unwrap();
                if t.row.is_none() {
                    t.row = Some(Vec::new());
                    t.row_th = true;
                }
                if name == "td" {
                    t.row_th = false;
                }
                let span: usize = attr("colspan").parse().unwrap_or(1).clamp(1, 64);
                for _ in 1..span {
                    t.row.as_mut().unwrap().push(String::new());
                }
                self.cell = Some(String::new());
            }
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        match name {
            "p" | "div" | "section" | "article" | "header" | "footer" | "figure" | "figcaption" | "dl" | "dt" | "dd"
            | "details" | "summary" | "center" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" => self.block(),
            "b" | "strong" if self.md() && self.pre == 0 => self.push("**"),
            "i" | "em" if self.md() && self.pre == 0 => self.push("*"),
            "s" | "del" | "strike" if self.md() && self.pre == 0 => self.push("~~"),
            "code" | "kbd" | "samp" | "tt" if self.md() && self.pre == 0 && self.code > 0 => {
                self.code -= 1;
                self.push("`");
            }
            "pre" if self.pre > 0 => {
                if self.md() && self.cell.is_none() {
                    let code = std::mem::take(&mut self.line);
                    self.line = format!("```\n{}\n```", code.trim_matches('\n'));
                }
                self.flush();
                self.pre -= 1;
            }
            "a" => {
                if let Some(href) = self.links.pop() {
                    if self.md() && self.pre == 0 && !href.is_empty() {
                        let h = href.replace(' ', "%20").replace(')', "%29");
                        self.push(&format!("]({h})"));
                    }
                }
            }
            "blockquote" if self.quote > 0 => {
                self.block();
                self.quote -= 1;
            }
            "ul" | "ol" if !self.lists.is_empty() => {
                self.block();
                self.lists.pop();
                if self.lists.is_empty() && self.cell.is_none() {
                    self.out.push('\n');
                }
            }
            "th" | "td" => self.end_cell(),
            "tr" => self.end_row(),
            "thead" => {
                self.end_row();
                if let Some(t) = self.tables.last_mut() {
                    t.in_head = false;
                }
            }
            "table" if !self.tables.is_empty() => self.end_table(),
            _ => {}
        }
    }

    fn end_cell(&mut self) {
        if let Some(c) = self.cell.take() {
            if let Some(t) = self.tables.last_mut() {
                let c = c.split_whitespace().collect::<Vec<_>>().join(" ");
                t.row.get_or_insert_with(Vec::new).push(c);
            }
        }
    }

    fn end_row(&mut self) {
        self.end_cell();
        if let Some(t) = self.tables.last_mut() {
            if let Some(r) = t.row.take() {
                if !r.is_empty() {
                    if (t.in_head || t.row_th) && t.head == t.rows.len() {
                        t.head += 1;
                    }
                    t.rows.push(r);
                }
            }
        }
    }

    fn end_table(&mut self) {
        self.end_row();
        let t = self.tables.pop().unwrap();
        if t.rows.is_empty() {
            return;
        }
        let cols = t.rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let rows: Vec<Vec<String>> = t.rows.into_iter().map(|mut r| {
            r.resize(cols, String::new());
            r
        }).collect();
        let text = if self.md() {
            // one header row: the first; the other header rows (pandas' index name row) are body rows
            let mut s = String::new();
            for (i, r) in rows.iter().enumerate() {
                s.push('|');
                for c in r {
                    s.push(' ');
                    s.push_str(c); // its | already escaped as text
                    s.push_str(" |");
                }
                s.push('\n');
                if i == 0 {
                    s.push('|');
                    for _ in 0..cols {
                        s.push_str(" --- |");
                    }
                    s.push('\n');
                }
            }
            s
        } else {
            let widths: Vec<usize> = (0..cols).map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0)).collect();
            let mut s = String::new();
            for (ri, r) in rows.iter().enumerate() {
                let mut l = String::new();
                for (i, c) in r.iter().enumerate() {
                    let pad = widths[i] - c.chars().count();
                    // numbers line up on the right, as pandas prints them
                    if ri >= t.head.max(1) && c.parse::<f64>().is_ok() {
                        l.push_str(&" ".repeat(pad));
                        l.push_str(c);
                    } else {
                        l.push_str(c);
                        l.push_str(&" ".repeat(pad));
                    }
                    l.push_str("  ");
                }
                s.push_str(l.trim_end());
                s.push('\n');
            }
            s
        };
        if let Some(cell) = self.cell.as_mut() {
            cell.push(' ');
            cell.push_str(&text.replace('\n', " "));
        } else {
            self.flush();
            self.out.push_str(&text);
            self.out.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: &str = r#"<div>
<style scoped>
    .dataframe tbody tr th:only-of-type { vertical-align: middle; }
</style>
<table border="1" class="dataframe">
  <thead>
    <tr style="text-align: right;">
      <th></th>
      <th>a</th>
      <th>b</th>
    </tr>
  </thead>
  <tbody>
    <tr>
      <th>0</th>
      <td>1</td>
      <td>x &amp; y</td>
    </tr>
    <tr>
      <th>1</th>
      <td>22</td>
      <td>z|w</td>
    </tr>
  </tbody>
</table>
</div>"#;

    #[test]
    fn a_dataframe_is_a_pipe_table() {
        assert_eq!(
            (to_markdown(FRAME)),
            "|  | a | b |\n| --- | --- | --- |\n| 0 | 1 | x & y |\n| 1 | 22 | z\\|w |\n"
        );
    }

    #[test]
    fn a_dataframe_as_text_lines_up() {
        assert_eq!((to_text(FRAME)), "   a   b\n0   1  x & y\n1  22  z|w\n");
    }

    #[test]
    fn blocks_and_inline() {
        let h = "<h2>Title</h2><p>Some <b>bold</b> and <i>it</i>, <code>x()</code>, a \
                 <a href=\"https://e.org/a b\">link</a>.<br>Next line</p><ul><li>one</li><li>two \
                 <ol><li>a</li><li>b</li></ol></li></ul><pre>  def f():\n      pass</pre><script>alert(1)</script>\
                 <!-- gone --><hr>";
        assert_eq!(
            (to_markdown(h)),
            "## Title\n\nSome **bold** and *it*, `x()`, a [link](https://e.org/a%20b).\\\nNext line\n\n\
             - one\n- two\n   1. a\n   2. b\n\n```\n  def f():\n      pass\n```\n\n---\n"
        );
        assert_eq!(
            (to_text(h)),
            "Title\n\nSome bold and it, x(), a link.\nNext line\n\n\u{2022} one\n\u{2022} two\n   1. a\n   2. b\n\n  def f():\n      pass\n\n----------\n"
        );
    }

    #[test]
    fn markdown_characters_in_text_are_escaped() {
        assert_eq!((to_markdown("<p>a*b_c [d] #e</p>")), "a\\*b\\_c \\[d\\] \\#e\n");
        assert_eq!((to_text("<p>a*b &lt;tag&gt; &#x41;&#66;</p>")), "a*b <tag> AB\n");
    }

    #[test]
    fn plain_text_and_broken_html() {
        assert_eq!(to_text("just text"), "just text\n");
        assert_eq!(to_text("<b>unclosed <i>tags"), "unclosed tags\n");
        assert_eq!(to_text("a < b and c > d"), "a < b and c > d\n");
        assert_eq!(to_text(""), "");
    }
}
