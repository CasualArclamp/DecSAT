// made by CasualArclamp
//! The beacon's bulletins (`.blt` files) as something to read.
//!
//! A bulletin is text with a first line like `~R:1:44~` or `~B:8:263~`.
//! HSmodem's own program hands the file to a web page as it is
//! (`oscardata/receivefile.cs`, `sendBulletin`) and its source says nothing
//! of a format, so this goes by the bulletins received in November 2026.
//! They come two ways:
//!
//! - plain text wrapped at about 76 columns, as an e-mail is: headings
//!   underlined with dashes, lists with `- `, web addresses broken across
//!   lines (the DARC's Deutschland-Rundspruch);
//! - a fragment of HTML (the AMSAT News Service).
//!
//! Plain text is un-wrapped: a line is joined to the next when the next
//! line's first word would not have fitted on it, which is how the lines
//! were broken in the first place. Short lines stay lines of their own.

use eframe::egui::{self, RichText, Ui};

use crate::ui::{ACCENT, MUTED, TEXT};

/// One piece of a bulletin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// The bulletin's name: its first line.
    Title(String),
    Heading(String),
    /// A paragraph, or a line that stands alone.
    Para(String),
    /// A list's item, its mark taken off.
    Item(String),
    /// A line across.
    Rule,
    /// The space between paragraphs.
    Gap,
}

/// A bulletin read into pieces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bulletin {
    /// What is between the tildes of the first line (`R:1:44`).
    pub tag: Option<String>,
    pub blocks: Vec<Block>,
    /// The pictures a web page carries inside itself (`data:` addresses),
    /// as the files they are. They are shown first: where on the page each
    /// belongs is the page's styling, which is not followed.
    pub images: Vec<Vec<u8>>,
}

/// A heading's mark in text that came from HTML.
const HEADING_MARK: char = '\u{1}';

/// Text with its HTML taken out. Line and paragraph ends become new lines.
pub(crate) fn plain(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[open + 1..open + close].to_ascii_lowercase();
        let name = tag.trim_start_matches('/');
        if ["br", "p", "h1", "h2", "h3", "h4", "li", "tr", "div"]
            .iter()
            .any(|t| {
                name == *t
                    || name.starts_with(&format!("{t} "))
                    || name.starts_with(&format!("{t}/"))
            })
            && !out.ends_with("\n\n")
        {
            out.push('\n');
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    for (from, to) in [
        ("&nbsp;", " "),
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
    ] {
        out = out.replace(from, to);
    }
    out.trim().to_string()
}

/// Base64 to bytes (RFC 4648's alphabet); `None` if it is not that.
fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.bytes().filter(|&c| c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The pictures written into a page as `data:image/…;base64,…`, and the
/// page without them (they are long, and may sit where text is kept).
fn take_images(html: &str) -> (String, Vec<Vec<u8>>) {
    const MARK: &str = "data:image/";
    let mut images = Vec::new();
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find(MARK) {
        out.push_str(&rest[..at]);
        let after = &rest[at + MARK.len()..];
        // "jpeg;base64," and then the picture, up to whatever closes it.
        let coded = after
            .find(";base64,")
            .filter(|&i| i <= 16)
            .map(|i| &after[i + 8..]);
        match coded {
            Some(coded) => {
                let end = coded
                    .find(|c: char| !(c.is_ascii_alphanumeric() || "+/=".contains(c)))
                    .unwrap_or(coded.len());
                if let Some(bytes) = base64(&coded[..end]).filter(|b| !b.is_empty()) {
                    images.push(bytes);
                }
                rest = &coded[end..];
            }
            None => {
                out.push_str(MARK);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    (out, images)
}

/// A page without what is not for reading: scripts, style sheets, comments
/// and the head, whose title is kept as a heading.
fn readable(html: &str) -> String {
    let mut text = html.to_string();
    let mut title = None;
    for (open, close) in [
        ("<!--", "-->"),
        ("<script", "</script>"),
        ("<style", "</style>"),
        ("<head", "</head>"),
    ] {
        loop {
            // Rust note: ASCII lower-casing keeps every byte where it was.
            let lower = text.to_ascii_lowercase();
            let Some(from) = lower.find(open) else { break };
            // Cut short (a page still arriving): to the end.
            let to = lower[from..]
                .find(close)
                .map_or(text.len(), |i| from + i + close.len());
            if open == "<head"
                && let Some(a) = lower[from..to].find("<title>")
                && let Some(b) = lower[from..to].find("</title>")
                && a + 7 <= b
            {
                title = Some(text[from + a + 7..from + b].trim().to_string());
            }
            text.replace_range(from..to, "");
        }
    }
    match title.filter(|t| !t.is_empty()) {
        Some(t) => format!("<h1>{t}</h1>{text}"),
        None => text,
    }
}

/// HTML to text, each heading on a line of its own behind [`HEADING_MARK`].
fn html_to_text(html: &str) -> String {
    // Rust note: ASCII lower-casing keeps every byte where it was, so
    // places found in `lower` are places in `html`.
    let lower = html.to_ascii_lowercase();
    let mut marked = String::with_capacity(html.len() + 64);
    let mut at = 0;
    while let Some(found) = lower[at..].find("<h") {
        let open = at + found;
        let level = lower.as_bytes().get(open + 2).copied().unwrap_or(0);
        let end_tag = format!("</h{}>", level as char);
        let inner = lower[open..].find('>').map(|i| open + i + 1);
        let close = inner.and_then(|i| lower[i..].find(&end_tag).map(|c| i + c));
        match (level, inner, close) {
            (b'1'..=b'4', Some(inner), Some(close)) => {
                marked.push_str(&html[at..open]);
                marked.push_str("<p>");
                marked.push(HEADING_MARK);
                marked.push_str(&html[inner..close]);
                marked.push_str("<p>");
                at = close + end_tag.len();
            }
            _ => {
                marked.push_str(&html[at..open + 2]);
                at = open + 2;
            }
        }
    }
    marked.push_str(&html[at..]);
    plain(&marked)
}

/// A line of dashes or equals signs only.
fn is_dashes(line: &str) -> bool {
    !line.is_empty() && line.chars().all(|c| c == '-' || c == '=')
}

/// A list item's text, if the line begins one.
fn item(line: &str) -> Option<&str> {
    line.strip_prefix("- ").or_else(|| line.strip_prefix("* "))
}

/// Read a bulletin's text.
pub fn parse(text: &str) -> Bulletin {
    let html = ["</", "<br", "<p>", "<p "].iter().any(|t| text.contains(t));
    let mut images = Vec::new();
    let text = if html {
        let (page, found) = take_images(text);
        images = found;
        html_to_text(&readable(&page))
    } else {
        text.replace('\r', "")
    };
    // A no-break space is a space here (the HTML has them after a list's
    // mark).
    let text = text.replace('\u{a0}', " ");
    let mut lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let mut out = Bulletin {
        images,
        ..Default::default()
    };
    // The first line with anything on it: the tag between tildes.
    if let Some(first) = lines.iter().position(|l| !l.is_empty()) {
        let l = lines[first].trim();
        if l.len() > 2 && l.starts_with('~') && l.ends_with('~') {
            out.tag = Some(l[1..l.len() - 1].to_string());
            lines.drain(..=first);
        }
    }
    // The width the text was wrapped at: its longest line.
    let width = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(40);
    for chunk in lines.split(|l| l.is_empty()).filter(|c| !c.is_empty()) {
        if !out.blocks.is_empty() {
            out.blocks.push(Block::Gap);
        }
        // Lines put back together: what has gathered since the last block.
        let mut logical: Vec<String> = Vec::new();
        // The length of the line before, as it was sent.
        let mut before_len = 0;
        let flush = |logical: &mut Vec<String>, blocks: &mut Vec<Block>| {
            for l in logical.drain(..) {
                blocks.push(match (l.strip_prefix(HEADING_MARK), item(&l)) {
                    (Some(h), _) => Block::Heading(h.trim().to_string()),
                    (None, Some(i)) => Block::Item(i.trim().to_string()),
                    (None, None) => Block::Para(l),
                });
            }
        };
        let mut i = 0;
        while i < chunk.len() {
            let line = chunk[i];
            if is_dashes(line) {
                // A run of dashes, itself wrapped perhaps.
                let mut run = 0;
                while i < chunk.len() && is_dashes(chunk[i]) {
                    run += chunk[i].len();
                    i += 1;
                }
                // Under text, and more than a mark: the text is a heading.
                if run >= 5 && !logical.is_empty() {
                    out.blocks.push(Block::Heading(logical.join(" ")));
                    logical.clear();
                } else {
                    flush(&mut logical, &mut out.blocks);
                    out.blocks.push(Block::Rule);
                }
                continue;
            }
            i += 1;
            // Joined to the line before when this line's first word would
            // not have fitted there. HTML's lines are as they are meant.
            let first_word = line.split_whitespace().next().unwrap_or("").chars().count();
            let fitted = before_len + 1 + first_word <= width;
            match logical.last_mut() {
                Some(before) if !html && item(line).is_none() && !fitted => {
                    // A web address cut at the full width, the rest of it
                    // alone on this line: no space was there.
                    let last_word = before.rsplit(' ').next().unwrap_or("");
                    let cut = before_len + 1 >= width
                        && last_word.contains("://")
                        && !line.contains(char::is_whitespace);
                    if !cut {
                        before.push(' ');
                    }
                    before.push_str(line.trim_start());
                }
                _ => logical.push(line.trim_start().to_string()),
            }
            before_len = line.chars().count();
        }
        flush(&mut logical, &mut out.blocks);
    }
    // The first line is the bulletin's name.
    if let Some(Block::Para(first) | Block::Heading(first)) = out.blocks.first().cloned()
        && first.chars().count() <= 80
    {
        out.blocks[0] = Block::Title(first);
    }
    out
}

/// Text on wrapped rows, its web addresses as links.
fn text(ui: &mut Ui, s: &str, size: f32) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let mut run = String::new();
        for word in s.split(' ') {
            if word.starts_with("http://") || word.starts_with("https://") {
                // Punctuation after an address is the sentence's.
                let url = word.trim_end_matches(['.', ',', ')', ';', ':']);
                ui.label(RichText::new(&run).size(size).color(TEXT));
                run.clear();
                ui.hyperlink_to(RichText::new(url).size(size), url);
                run.push_str(&word[url.len()..]);
            } else {
                run.push_str(word);
            }
            run.push(' ');
        }
        ui.label(RichText::new(run.trim_end()).size(size).color(TEXT));
    });
}

/// A bulletin's pictures as textures, made the first time it is drawn
/// (`None` for one that would not decode).
#[derive(Default)]
pub struct Pictures(Option<Vec<Option<egui::TextureHandle>>>);

/// The largest picture taken, a side (pixels): these come off the air.
const PICTURE_MAX: u32 = 4096;

impl Pictures {
    fn of(&mut self, ui: &Ui, b: &Bulletin) -> &[Option<egui::TextureHandle>] {
        self.0.get_or_insert_with(|| {
            b.images
                .iter()
                .enumerate()
                .map(|(i, bytes)| {
                    let picture = image::load_from_memory(bytes).ok()?.into_rgba8();
                    let (w, h) = picture.dimensions();
                    if w == 0 || h == 0 || w > PICTURE_MAX || h > PICTURE_MAX {
                        return None;
                    }
                    let colour = egui::ColorImage::from_rgba_unmultiplied(
                        [w as usize, h as usize],
                        picture.as_raw(),
                    );
                    Some(ui.ctx().load_texture(
                        format!("bulletin-{i}"),
                        colour,
                        egui::TextureOptions::LINEAR,
                    ))
                })
                .collect()
        })
    }
}

/// Draw a bulletin.
pub fn show(ui: &mut Ui, b: &Bulletin, pictures: &mut Pictures) {
    ui.spacing_mut().item_spacing.y = 2.0;
    for texture in pictures.of(ui, b).iter().flatten() {
        // Its own size, or the panel's width if that is less.
        let size = texture.size_vec2();
        let fit = (ui.available_width() / size.x).min(1.0);
        ui.image((texture.id(), size * fit));
        ui.add_space(4.0);
    }
    if let Some(tag) = &b.tag {
        ui.label(
            RichText::new(format!("bulletin {tag}"))
                .size(11.0)
                .color(MUTED),
        );
    }
    for block in &b.blocks {
        match block {
            Block::Title(t) => {
                ui.label(RichText::new(t).size(17.0).strong().color(TEXT));
                ui.add_space(2.0);
            }
            Block::Heading(h) => {
                ui.add_space(4.0);
                ui.label(RichText::new(h).size(14.0).strong().color(ACCENT));
                ui.add_space(1.0);
            }
            Block::Para(p) => text(ui, p, 12.5),
            Block::Item(i) => {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new("  \u{2022}").size(12.5).color(MUTED));
                    ui.vertical(|ui| text(ui, i, 12.5));
                });
            }
            Block::Rule => {
                ui.add_space(3.0);
                ui.add(egui::Separator::default().spacing(1.0));
                ui.add_space(3.0);
            }
            Block::Gap => ui.add_space(7.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // From the DARC's Deutschland-Rundspruch 44/2026, as the beacon sent it
    // on 10 November 2026: the start, a section and the list of addresses.
    const RUNDSPRUCH: &str = "~R:1:44~

DARC DL-Rundspruch 44/2026



DARC e.V., Lindenallee 4, 34225 Baunatal, Telefon 0561 949880
Deutschland-Rundspruch 44/2026, 45. KW

Hallo liebe SWLs, YLs und OMs,
Sie hören den Deutschland-Rundspruch Nummer 44 des Deutschen
Amateur-Radio-Clubs für die 45. Kalenderwoche 2026. Diesmal haben wir
Meldungen zu folgenden Themen:

- Neue Schnittstellenbeschreibung für Amateurfunkanlagen - Verfügung Nr.
77/2026
- Reverse Beacon Network von der Yasme-Stiftung ausgezeichnet
- Aktuelle Conteste
und
- Was gibt es Neues vom Funkwetter?

Neue Schnittstellenbeschreibung für Amateurfunkanlagen - Verfügung Nr.
77/2026
----------------------------------------------------------------------------
--
Mit der Verfügung Nr. 77/2026 tritt die neue Schnittstellenbeschreibung SSB
SF 008 für kommerziell verfügbare Amateurfunkanlagen im Frequenzbereich bis
275 GHz mit sofortiger Wirkung in Kraft.

Aktuelle Conteste
-----------------
7. November: 50. IPA Radio Club Contest
7. bis 8. November: IARU-Region 1 Marconi Contest VHF

---
Verzeichnis der Internetadressen (Rundspruchsprecher: Bitte nicht
vorlesen!):
[1] https://www.bundesnetzagentur.de
[3]
https://www.rtl.lu/news/national/de-6-november-ginn-dmasten-zu-beidler-gespr
engt-2099246245.

Mit freundlichen Grüßen / Kind regards
Redaktion  CQ DL
--
Deutscher Amateur-Radio-Club e. V.
";

    #[test]
    fn a_wrapped_text_bulletin() {
        let b = parse(RUNDSPRUCH);
        assert_eq!(b.tag.as_deref(), Some("R:1:44"));
        assert_eq!(
            b.blocks[0],
            Block::Title("DARC DL-Rundspruch 44/2026".into())
        );
        let has = |want: &Block| b.blocks.contains(want);
        // Short lines stay apart; a wrapped sentence is one paragraph.
        assert!(has(&Block::Para("Hallo liebe SWLs, YLs und OMs,".into())));
        assert!(has(&Block::Para(
            "Sie hören den Deutschland-Rundspruch Nummer 44 des Deutschen \
             Amateur-Radio-Clubs für die 45. Kalenderwoche 2026. Diesmal haben wir \
             Meldungen zu folgenden Themen:"
                .into()
        )));
        // A list, one item wrapped; the lone "und" is not part of an item.
        assert!(has(&Block::Item(
            "Neue Schnittstellenbeschreibung für Amateurfunkanlagen - Verfügung Nr. 77/2026".into()
        )));
        assert!(has(&Block::Item("Aktuelle Conteste".into())));
        assert!(has(&Block::Para("und".into())));
        // Headings: two lines over a wrapped rule, and one line.
        assert!(has(&Block::Heading(
            "Neue Schnittstellenbeschreibung für Amateurfunkanlagen - Verfügung Nr. 77/2026".into()
        )));
        assert!(has(&Block::Heading("Aktuelle Conteste".into())));
        assert!(has(&Block::Para(
            "7. November: 50. IPA Radio Club Contest".into()
        )));
        // A rule, not a heading, where dashes stand alone or are a mark.
        assert_eq!(b.blocks.iter().filter(|x| **x == Block::Rule).count(), 2);
        assert!(has(&Block::Para("Redaktion  CQ DL".into())));
        // The broken address is whole again.
        assert!(has(&Block::Para(
            "[1] https://www.bundesnetzagentur.de".into()
        )));
        assert!(has(&Block::Para(
            "[3] https://www.rtl.lu/news/national/de-6-november-ginn-dmasten-zu-beidler-gesprengt-2099246245."
                .into()
        )));
    }

    #[test]
    fn an_html_bulletin() {
        let html = "~B:8:263~\n<h3 style=\"color:#1a639b\">AMSAT NEWS SERVICE ANS-263</h3>\n<p style=\"font-size:12px\">\n\nIn this edition:</span></font></p>\n<p>*\u{a0}One &amp; two<br>* Three</p><h3>AO-7 Again</h3><p>Text of the item.</p>";
        let b = parse(html);
        assert_eq!(b.tag.as_deref(), Some("B:8:263"));
        assert_eq!(
            b.blocks[0],
            Block::Title("AMSAT NEWS SERVICE ANS-263".into())
        );
        assert!(b.blocks.contains(&Block::Para("In this edition:".into())));
        assert!(b.blocks.contains(&Block::Item("One & two".into())));
        assert!(b.blocks.contains(&Block::Item("Three".into())));
        assert!(b.blocks.contains(&Block::Heading("AO-7 Again".into())));
        assert!(b.blocks.contains(&Block::Para("Text of the item.".into())));
    }

    #[test]
    fn a_web_page_is_read_without_its_scripts_and_styles() {
        // A one-pixel PNG in a style sheet, as the beacon's own page has
        // its banner.
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
        let page = format!(
            "<!DOCTYPE html><html><head><title>QO-100 Info</title>\n<style>\n.top {{ background-image:url('data:image/png;base64,{png}'); }}\n</style>\n<script>\nvar sockurl = \"ws://\" + \"localhost\";\nfunction go() {{ if (a < b) {{ c(); }} }}\n</script></head>\n<body><!-- a comment --><h2>DX Cluster</h2><p>Latest spots</p>\n<script>websocket.onopen = function () {{}};</script></body></html>"
        );
        let b = parse(&page);
        assert_eq!(b.images.len(), 1);
        assert_eq!(b.images[0][..8], *b"\x89PNG\r\n\x1a\n");
        assert_eq!(
            b.blocks,
            vec![
                Block::Title("QO-100 Info".into()),
                Block::Gap,
                Block::Heading("DX Cluster".into()),
                Block::Gap,
                Block::Para("Latest spots".into()),
            ]
        );
        // Cut off in the middle of its script, as while it arrives.
        let part = parse(&page[..page.find("function go").unwrap()]);
        assert_eq!(part.blocks, vec![Block::Title("QO-100 Info".into())]);
    }

    #[test]
    fn html_becomes_lines() {
        let text = plain("<p>In this edition:<br>* One &amp; two<br>* Three</p>");
        assert!(text.contains("* One & two\n* Three"));
        assert!(!text.contains('<'));
    }
}
