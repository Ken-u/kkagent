//! Terminal markdown renderer for transcript display (kimi-code aligned).
//!
//! Parses markdown with `pulldown-cmark` and emits styled `ratatui` lines.
//! Display-only — does not mutate LLM / session content.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::Theme;

/// Render markdown into width-wrapped styled lines (content only, no ● indent).
pub fn render(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(8);
    if text.trim().is_empty() {
        return Vec::new();
    }
    let normalized = text.replace('\t', "   ");
    let mut writer = MdWriter::new(width, theme);
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    for event in Parser::new_ext(&normalized, opts) {
        writer.handle(event);
    }
    writer.finish()
}

struct Styles {
    text: Style,
    heading: Style,
    heading1: Style,
    strong: Style,
    emphasis: Style,
    strike: Style,
    code: Style,
    code_block: Style,
    code_fence: Style,
    code_comment: Style,
    quote: Style,
    quote_border: Style,
    hr: Style,
    list_bullet: Style,
    link: Style,
    link_url: Style,
    table_border: Style,
    table_header: Style,
    html_comment: Style,
}

impl Styles {
    fn from_theme(theme: &Theme) -> Self {
        Self {
            text: Style::default().fg(theme.text),
            heading: Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
            heading1: Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            strong: Style::default()
                .fg(theme.text_strong)
                .add_modifier(Modifier::BOLD),
            emphasis: Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::ITALIC),
            strike: Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::CROSSED_OUT),
            // kimi: yellow inline code
            code: Style::default().fg(theme.warning),
            // kimi: green code body
            code_block: Style::default().fg(theme.success),
            code_fence: Style::default().fg(theme.text_muted),
            code_comment: Style::default()
                .fg(theme.text_muted)
                .add_modifier(Modifier::ITALIC),
            quote: Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::ITALIC),
            quote_border: Style::default().fg(theme.text_muted),
            hr: Style::default().fg(theme.text_muted),
            list_bullet: Style::default().fg(theme.accent),
            link: Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::UNDERLINED),
            link_url: Style::default().fg(theme.text_muted),
            table_border: Style::default().fg(theme.text_muted),
            table_header: Style::default()
                .fg(theme.text_strong)
                .add_modifier(Modifier::BOLD),
            html_comment: Style::default()
                .fg(theme.text_muted)
                .add_modifier(Modifier::ITALIC),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InlineKind {
    Strong,
    Emphasis,
    Strike,
    Link,
}

struct ListCtx {
    ordered: bool,
    next_num: u64,
    indent: usize,
}

#[derive(Clone, Default)]
struct TableCell {
    spans: Vec<Span<'static>>,
}

impl TableCell {
    fn plain(&self) -> String {
        self.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    fn width(&self) -> usize {
        UnicodeWidthStr::width(self.plain().as_str())
    }
}

struct TableState {
    header: Vec<TableCell>,
    rows: Vec<Vec<TableCell>>,
    current_row: Vec<TableCell>,
    current_cell: TableCell,
    in_header: bool,
}

struct MdWriter {
    width: usize,
    styles: Styles,
    out: Vec<Line<'static>>,
    line: Vec<Span<'static>>,
    inline_stack: Vec<InlineKind>,
    lists: Vec<ListCtx>,
    pending_marker: Option<Vec<Span<'static>>>,
    blockquote_depth: usize,
    in_code_block: bool,
    code_lang: String,
    code_buf: String,
    heading_level: Option<u8>,
    table: Option<TableState>,
    link_href: Option<String>,
    link_text: String,
    skip_image: bool,
}

impl MdWriter {
    fn new(width: usize, theme: &Theme) -> Self {
        Self {
            width,
            styles: Styles::from_theme(theme),
            out: Vec::new(),
            line: Vec::new(),
            inline_stack: Vec::new(),
            lists: Vec::new(),
            pending_marker: None,
            blockquote_depth: 0,
            in_code_block: false,
            code_lang: String::new(),
            code_buf: String::new(),
            heading_level: None,
            table: None,
            link_href: None,
            link_text: String::new(),
            skip_image: false,
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line();
        // Drop trailing blank lines
        while self
            .out
            .last()
            .is_some_and(|l| l.spans.is_empty() || line_is_blank(l))
        {
            self.out.pop();
        }
        self.out
    }

    fn handle(&mut self, event: Event<'_>) {
        if self.skip_image {
            match event {
                Event::End(TagEnd::Image) => self.skip_image = false,
                Event::Text(t) => {
                    // Use alt text as fallback display.
                    self.push_text(&t);
                }
                _ => {}
            }
            return;
        }

        if self.in_code_block {
            match event {
                Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => {
                    self.code_buf.push_str(&t);
                }
                Event::End(TagEnd::CodeBlock) => {
                    self.flush_code_block();
                    self.in_code_block = false;
                    self.code_lang.clear();
                    self.code_buf.clear();
                }
                _ => {}
            }
            return;
        }

        if self.table.is_some() {
            match &event {
                Event::Start(Tag::TableHead) => {
                    if let Some(t) = self.table.as_mut() {
                        t.in_header = true;
                    }
                }
                Event::End(TagEnd::TableHead) => {
                    if let Some(t) = self.table.as_mut() {
                        t.in_header = false;
                    }
                }
                Event::Start(Tag::TableRow) => {
                    if let Some(t) = self.table.as_mut() {
                        t.current_row.clear();
                    }
                }
                Event::End(TagEnd::TableRow) => {
                    if let Some(t) = self.table.as_mut() {
                        let row = std::mem::take(&mut t.current_row);
                        if t.in_header || t.header.is_empty() {
                            if t.header.is_empty() {
                                t.header = row;
                            }
                        } else {
                            t.rows.push(row);
                        }
                    }
                }
                Event::Start(Tag::TableCell) => {
                    if let Some(t) = self.table.as_mut() {
                        t.current_cell = TableCell { spans: Vec::new() };
                    }
                }
                Event::End(TagEnd::TableCell) => {
                    if let Some(t) = self.table.as_mut() {
                        let cell = std::mem::take(&mut t.current_cell);
                        if t.in_header {
                            t.header.push(cell);
                        } else {
                            t.current_row.push(cell);
                        }
                    }
                }
                Event::End(TagEnd::Table) => {
                    let table = self.table.take().expect("table");
                    self.emit_table(table);
                }
                Event::Text(t) => {
                    let style = self.current_style();
                    if let Some(table) = self.table.as_mut() {
                        table
                            .current_cell
                            .spans
                            .push(Span::styled(t.to_string(), style));
                    }
                }
                Event::Code(t) => {
                    let style = self.styles.code;
                    if let Some(table) = self.table.as_mut() {
                        table
                            .current_cell
                            .spans
                            .push(Span::styled(t.to_string(), style));
                    }
                }
                Event::SoftBreak | Event::HardBreak => {
                    if let Some(table) = self.table.as_mut() {
                        table.current_cell.spans.push(Span::raw(" "));
                    }
                }
                Event::Start(Tag::Strong) => self.inline_stack.push(InlineKind::Strong),
                Event::End(TagEnd::Strong) => {
                    self.inline_stack.pop();
                }
                Event::Start(Tag::Emphasis) => self.inline_stack.push(InlineKind::Emphasis),
                Event::End(TagEnd::Emphasis) => {
                    self.inline_stack.pop();
                }
                Event::Start(Tag::Strikethrough) => self.inline_stack.push(InlineKind::Strike),
                Event::End(TagEnd::Strikethrough) => {
                    self.inline_stack.pop();
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    self.link_href = Some(dest_url.to_string());
                    self.inline_stack.push(InlineKind::Link);
                }
                Event::End(TagEnd::Link) => {
                    self.inline_stack.pop();
                    self.link_href = None;
                }
                _ => {}
            }
            return;
        }

        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(t) => self.push_text(&t),
            Event::Code(t) => {
                self.push_span(Span::styled(t.to_string(), self.styles.code));
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.flush_line();
                let n = self.content_width().min(80);
                self.out
                    .push(Line::from(Span::styled("─".repeat(n), self.styles.hr)));
                self.push_blank();
            }
            Event::Html(html) | Event::InlineHtml(html) => self.push_html(&html),
            Event::FootnoteReference(name) => {
                self.push_span(Span::styled(format!("[{name}]"), self.styles.link_url));
            }
            Event::TaskListMarker(checked) => {
                let mark = if checked { "[x] " } else { "[ ] " };
                self.push_span(Span::styled(mark.to_string(), self.styles.list_bullet));
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                self.push_span(Span::styled(t.to_string(), self.styles.code));
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.heading_level = Some(level as u8);
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.in_code_block = true;
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code_buf.clear();
            }
            Tag::List(start) => {
                self.flush_line();
                let indent = self.lists.len();
                let ordered = start.is_some();
                let next_num = start.unwrap_or(1);
                self.lists.push(ListCtx {
                    ordered,
                    next_num,
                    indent,
                });
            }
            Tag::Item => {
                self.flush_line();
                if let Some(list) = self.lists.last_mut() {
                    let indent = "    ".repeat(list.indent);
                    let marker = if list.ordered {
                        let n = list.next_num;
                        list.next_num += 1;
                        format!("{n}. ")
                    } else {
                        "- ".to_string()
                    };
                    let mut spans = Vec::new();
                    if !indent.is_empty() {
                        spans.push(Span::raw(indent));
                    }
                    spans.push(Span::styled(marker, self.styles.list_bullet));
                    self.pending_marker = Some(spans);
                }
            }
            Tag::Table(_) => {
                self.flush_line();
                self.table = Some(TableState {
                    header: Vec::new(),
                    rows: Vec::new(),
                    current_row: Vec::new(),
                    current_cell: TableCell { spans: Vec::new() },
                    in_header: false,
                });
            }
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
            Tag::Emphasis => self.inline_stack.push(InlineKind::Emphasis),
            Tag::Strong => self.inline_stack.push(InlineKind::Strong),
            Tag::Strikethrough => self.inline_stack.push(InlineKind::Strike),
            Tag::Link { dest_url, .. } => {
                self.link_href = Some(dest_url.to_string());
                self.link_text.clear();
                self.inline_stack.push(InlineKind::Link);
            }
            Tag::Image { .. } => {
                self.skip_image = true;
            }
            Tag::HtmlBlock => {}
            Tag::FootnoteDefinition(_) => {}
            Tag::MetadataBlock(_) => {}
            Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {}
            Tag::Superscript | Tag::Subscript => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                if self.lists.is_empty() {
                    self.push_blank();
                }
            }
            TagEnd::Heading(_) => {
                // h3+ keep visible hashes like kimi
                if let Some(level) = self.heading_level {
                    if level >= 3 {
                        let prefix = format!("{} ", "#".repeat(level as usize));
                        let mut spans = vec![Span::styled(prefix, self.styles.heading)];
                        spans.append(&mut self.line);
                        self.line = spans;
                    }
                }
                self.heading_level = None;
                self.flush_line();
                self.push_blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.push_blank();
            }
            TagEnd::CodeBlock => {}
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.push_blank();
                }
            }
            TagEnd::Item => {
                self.flush_line();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.inline_stack.pop();
            }
            TagEnd::Link => {
                if let Some(href) = self.link_href.take() {
                    let label = std::mem::take(&mut self.link_text);
                    let href_cmp = href.strip_prefix("mailto:").unwrap_or(&href);
                    if !label.is_empty() && label != href && label != href_cmp {
                        self.push_span(Span::styled(format!(" ({href})"), self.styles.link_url));
                    }
                }
                self.inline_stack.pop();
            }
            TagEnd::Image => self.skip_image = false,
            TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript => {}
        }
    }

    fn push_html(&mut self, html: &str) {
        let trimmed = html.trim();
        if trimmed.starts_with("<!--") {
            let body = trimmed
                .trim_start_matches("<!--")
                .trim_end_matches("-->")
                .trim();
            if !body.is_empty() {
                self.flush_line();
                self.push_span(Span::styled(
                    format!("/* {body} */"),
                    self.styles.html_comment,
                ));
                self.flush_line();
            }
            return;
        }
        // Strip other HTML tags for display; keep text-ish content.
        let plain = strip_simple_html(trimmed);
        if !plain.is_empty() {
            self.push_text(&plain);
        }
    }

    fn current_style(&self) -> Style {
        if self.heading_level == Some(1) {
            return self.styles.heading1;
        }
        if self.heading_level.is_some() {
            return self.styles.heading;
        }
        if self.blockquote_depth > 0 && self.inline_stack.is_empty() {
            return self.styles.quote;
        }
        let mut style = self.styles.text;
        for kind in &self.inline_stack {
            match kind {
                InlineKind::Strong => style = style.patch(self.styles.strong),
                InlineKind::Emphasis => style = style.patch(self.styles.emphasis),
                InlineKind::Strike => style = style.patch(self.styles.strike),
                InlineKind::Link => style = style.patch(self.styles.link),
            }
        }
        if self.blockquote_depth > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        style
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.link_href.is_some() {
            self.link_text.push_str(text);
        }
        self.push_span(Span::styled(text.to_string(), self.current_style()));
    }

    fn push_span(&mut self, span: Span<'static>) {
        if span.content.is_empty() {
            return;
        }
        self.line.push(span);
    }

    fn content_width(&self) -> usize {
        let quote = self.blockquote_depth.saturating_mul(2);
        self.width.saturating_sub(quote).max(8)
    }

    fn flush_line(&mut self) {
        if self.line.is_empty() && self.pending_marker.is_none() {
            return;
        }

        let mut prefix: Vec<Span<'static>> = Vec::new();
        for _ in 0..self.blockquote_depth {
            prefix.push(Span::styled("│ ".to_string(), self.styles.quote_border));
        }
        if let Some(marker) = self.pending_marker.take() {
            prefix.extend(marker);
        }

        let prefix_w: usize = prefix
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let avail = self.width.saturating_sub(prefix_w).max(1);
        let wrapped = wrap_spans(&self.line, avail);
        self.line.clear();

        let cont_indent = {
            let mut w = 0usize;
            for _ in 0..self.blockquote_depth {
                w += 2;
            }
            // Continue under list marker: indent by marker visible width.
            if prefix_w > w {
                " ".repeat(prefix_w)
            } else if self.blockquote_depth > 0 {
                let mut p = Vec::new();
                for _ in 0..self.blockquote_depth {
                    p.push(Span::styled("│ ".to_string(), self.styles.quote_border));
                }
                // handled below via cont_prefix
                String::new()
            } else {
                String::new()
            }
        };

        for (i, chunk) in wrapped.into_iter().enumerate() {
            let mut spans = Vec::new();
            if i == 0 {
                spans.extend(prefix.iter().cloned());
            } else if self.blockquote_depth > 0 && prefix_w == self.blockquote_depth * 2 {
                for _ in 0..self.blockquote_depth {
                    spans.push(Span::styled("│ ".to_string(), self.styles.quote_border));
                }
            } else if !cont_indent.is_empty() {
                spans.push(Span::raw(cont_indent.clone()));
            } else if self.blockquote_depth > 0 {
                for _ in 0..self.blockquote_depth {
                    spans.push(Span::styled("│ ".to_string(), self.styles.quote_border));
                }
            }
            spans.extend(chunk);
            self.out.push(Line::from(spans));
        }
    }

    fn push_blank(&mut self) {
        if self.out.last().is_some_and(line_is_blank) {
            return;
        }
        self.out.push(Line::from(""));
    }

    fn flush_code_block(&mut self) {
        let lang = self.code_lang.trim();
        let fence = if lang.is_empty() {
            "```".to_string()
        } else {
            format!("```{lang}")
        };
        self.out
            .push(Line::from(Span::styled(fence, self.styles.code_fence)));

        // Trim trailing newline that pulldown includes.
        let body = self.code_buf.trim_end_matches('\n');
        if body.is_empty() {
            self.out.push(Line::from(Span::styled(
                "  ".to_string(),
                self.styles.code_block,
            )));
        } else {
            for raw in body.lines() {
                let styled = style_code_line(raw, lang, &self.styles);
                let spans = vec![Span::raw("  ")];
                // Soft-wrap long code lines
                let avail = self.width.saturating_sub(2).max(1);
                let wrapped = wrap_spans(&styled, avail);
                for (i, chunk) in wrapped.into_iter().enumerate() {
                    if i == 0 {
                        let mut line = spans.clone();
                        line.extend(chunk);
                        self.out.push(Line::from(line));
                    } else {
                        let mut line = vec![Span::raw("  ")];
                        line.extend(chunk);
                        self.out.push(Line::from(line));
                    }
                }
            }
        }
        self.out.push(Line::from(Span::styled(
            "```".to_string(),
            self.styles.code_fence,
        )));
        self.push_blank();
    }

    fn emit_table(&mut self, table: TableState) {
        let num_cols = table
            .header
            .len()
            .max(table.rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if num_cols == 0 {
            return;
        }

        // Normalize
        let mut header = table.header;
        while header.len() < num_cols {
            header.push(TableCell { spans: Vec::new() });
        }
        header.truncate(num_cols);
        let mut rows = table.rows;
        for row in &mut rows {
            while row.len() < num_cols {
                row.push(TableCell { spans: Vec::new() });
            }
            row.truncate(num_cols);
        }

        // border overhead: "│ " + (n-1)*" │ " + " │" = 3n + 1
        let border_overhead = 3 * num_cols + 1;
        let available_for_cells = self.width.saturating_sub(border_overhead);
        if available_for_cells < num_cols {
            // Fallback: pipe format
            self.emit_table_pipe(&header, &rows);
            self.push_blank();
            return;
        }

        let mut natural: Vec<usize> = (0..num_cols)
            .map(|i| header.get(i).map(|c| c.width()).unwrap_or(1).max(1))
            .collect();
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                natural[i] = natural[i].max(cell.width()).max(1);
            }
        }

        let total_natural: usize = natural.iter().sum();
        let col_widths = if total_natural <= available_for_cells {
            natural
        } else {
            // Proportional shrink, floor 1
            let mut widths = vec![1usize; num_cols];
            let remaining = available_for_cells.saturating_sub(num_cols);
            let weight: usize = natural.iter().map(|w| w.saturating_sub(1)).sum();
            if weight > 0 && remaining > 0 {
                let mut allocated = 0usize;
                for i in 0..num_cols {
                    let grow = (natural[i].saturating_sub(1) * remaining) / weight;
                    widths[i] += grow;
                    allocated += grow;
                }
                let mut leftover = remaining.saturating_sub(allocated);
                let mut i = 0;
                while leftover > 0 && i < num_cols * 2 {
                    widths[i % num_cols] += 1;
                    leftover -= 1;
                    i += 1;
                }
            }
            widths
        };

        // Top border
        let top = format!(
            "┌─{}─┐",
            col_widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("─┬─")
        );
        self.out
            .push(Line::from(Span::styled(top, self.styles.table_border)));

        // Header (may wrap)
        self.emit_table_row(&header, &col_widths, true);

        let sep = format!(
            "├─{}─┤",
            col_widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("─┼─")
        );
        self.out.push(Line::from(Span::styled(
            sep.clone(),
            self.styles.table_border,
        )));

        for (ri, row) in rows.iter().enumerate() {
            self.emit_table_row(row, &col_widths, false);
            if ri + 1 < rows.len() {
                self.out.push(Line::from(Span::styled(
                    sep.clone(),
                    self.styles.table_border,
                )));
            }
        }

        let bottom = format!(
            "└─{}─┘",
            col_widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("─┴─")
        );
        self.out
            .push(Line::from(Span::styled(bottom, self.styles.table_border)));
        self.push_blank();
    }

    fn emit_table_row(&mut self, cells: &[TableCell], widths: &[usize], is_header: bool) {
        let wrapped: Vec<Vec<Vec<Span<'static>>>> = cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let mut spans = cell.spans.clone();
                if is_header {
                    for s in &mut spans {
                        s.style = self.styles.table_header;
                    }
                }
                wrap_spans(&spans, widths[i].max(1))
            })
            .collect();
        let row_h = wrapped.iter().map(|c| c.len()).max().unwrap_or(1).max(1);
        for li in 0..row_h {
            let mut spans = vec![Span::styled("│ ".to_string(), self.styles.table_border)];
            for (ci, cell_lines) in wrapped.iter().enumerate() {
                if ci > 0 {
                    spans.push(Span::styled(" │ ".to_string(), self.styles.table_border));
                }
                let chunk = cell_lines.get(li).cloned().unwrap_or_default();
                let w: usize = chunk
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum();
                let pad = widths[ci].saturating_sub(w);
                spans.extend(chunk);
                if pad > 0 {
                    spans.push(Span::raw(" ".repeat(pad)));
                }
            }
            spans.push(Span::styled(" │".to_string(), self.styles.table_border));
            self.out.push(Line::from(spans));
        }
    }

    fn emit_table_pipe(&mut self, header: &[TableCell], rows: &[Vec<TableCell>]) {
        let fmt = |cells: &[TableCell]| {
            format!(
                "| {} |",
                cells
                    .iter()
                    .map(|c| c.plain())
                    .collect::<Vec<_>>()
                    .join(" | ")
            )
        };
        let header_line = fmt(header);
        for chunk in wrap_str(&header_line, self.width) {
            self.out
                .push(Line::from(Span::styled(chunk, self.styles.table_header)));
        }
        let sep = format!(
            "|{}|",
            (0..header.len())
                .map(|_| "---")
                .collect::<Vec<_>>()
                .join("|")
        );
        self.out
            .push(Line::from(Span::styled(sep, self.styles.table_border)));
        for row in rows {
            let line = fmt(row);
            for chunk in wrap_str(&line, self.width) {
                self.out
                    .push(Line::from(Span::styled(chunk, self.styles.text)));
            }
        }
    }
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.is_empty()
        || line
            .spans
            .iter()
            .all(|s| s.content.chars().all(|c| c.is_whitespace()))
}

fn strip_simple_html(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Style a code line: dim+italic for comments (kimi-like readability).
fn style_code_line(line: &str, lang: &str, styles: &Styles) -> Vec<Span<'static>> {
    let lang = lang.to_ascii_lowercase();
    let hash_comment = matches!(
        lang.as_str(),
        "" | "sh"
            | "bash"
            | "zsh"
            | "shell"
            | "py"
            | "python"
            | "rb"
            | "ruby"
            | "yaml"
            | "yml"
            | "toml"
            | "dockerfile"
            | "make"
            | "makefile"
            | "r"
            | "perl"
    );
    let c_comment = matches!(
        lang.as_str(),
        "" | "rs"
            | "rust"
            | "js"
            | "javascript"
            | "ts"
            | "typescript"
            | "tsx"
            | "jsx"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "java"
            | "kt"
            | "go"
            | "swift"
            | "cs"
            | "php"
            | "css"
            | "scss"
    );

    let trimmed = line.trim_start();
    let indent_len = line.len() - trimmed.len();
    let indent = line[..indent_len].to_string();

    let is_full_comment = (hash_comment && trimmed.starts_with('#'))
        || (c_comment
            && (trimmed.starts_with("//")
                || trimmed.starts_with("/*")
                || trimmed.starts_with('*')))
        || (lang == "html" || lang == "xml" || lang == "svg") && trimmed.starts_with("<!--");

    if is_full_comment {
        let mut spans = Vec::new();
        if !indent.is_empty() {
            spans.push(Span::styled(indent, styles.code_block));
        }
        spans.push(Span::styled(trimmed.to_string(), styles.code_comment));
        return spans;
    }

    // Trailing // or # comments
    if let Some(idx) = find_trailing_line_comment(line, hash_comment, c_comment) {
        let (code, comment) = line.split_at(idx);
        return vec![
            Span::styled(code.to_string(), styles.code_block),
            Span::styled(comment.to_string(), styles.code_comment),
        ];
    }

    vec![Span::styled(line.to_string(), styles.code_block)]
}

fn find_trailing_line_comment(line: &str, hash: bool, c_style: bool) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut in_str: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' {
            in_str = Some(b);
            i += 1;
            continue;
        }
        if c_style && b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            return Some(i);
        }
        if hash && b == b'#' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn wrap_spans(spans: &[Span<'static>], max_width: usize) -> Vec<Vec<Span<'static>>> {
    if max_width == 0 {
        return vec![spans.to_vec()];
    }
    if spans.is_empty() {
        return vec![Vec::new()];
    }

    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;

    for span in spans {
        let style = span.style;
        let content = span.content.as_ref();
        for ch in content.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if ch == '\n' {
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
                continue;
            }
            if cur_w + w > max_width && !cur.is_empty() {
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            if let Some(last) = cur.last_mut() {
                if last.style == style {
                    last.content.to_mut().push(ch);
                } else {
                    cur.push(Span::styled(ch.to_string(), style));
                }
            } else {
                cur.push(Span::styled(ch.to_string(), style));
            }
            cur_w += w;
        }
    }
    if !cur.is_empty() || rows.is_empty() {
        rows.push(cur);
    }
    rows
}

fn wrap_str(s: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + w > max_width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += w;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn renders_bold_and_inline_code() {
        let theme = Theme::default();
        let lines = render("Hello **world** and `code`.", 80, &theme);
        let text = plain(&lines).join("\n");
        assert!(text.contains("world"));
        assert!(text.contains("code"));
        assert!(!text.contains("**"));
        assert!(!text.contains('`'));
    }

    #[test]
    fn renders_table_box() {
        let theme = Theme::default();
        let md = "| A | B |\n| --- | --- |\n| 1 | 2 |\n";
        let lines = render(md, 80, &theme);
        let text = plain(&lines).join("\n");
        assert!(text.contains('┌'));
        assert!(text.contains('│'));
        assert!(text.contains('└'));
        assert!(text.contains('A'));
        assert!(text.contains('1'));
    }

    #[test]
    fn renders_code_block_with_fence() {
        let theme = Theme::default();
        let md = "```rust\n// comment\nlet x = 1;\n```\n";
        let lines = render(md, 80, &theme);
        let text = plain(&lines).join("\n");
        assert!(text.contains("```rust"));
        assert!(text.contains("// comment"));
        assert!(text.contains("let x = 1;"));
    }

    #[test]
    fn renders_blockquote() {
        let theme = Theme::default();
        let lines = render("> note\n", 80, &theme);
        let text = plain(&lines).join("\n");
        assert!(text.contains('│'));
        assert!(text.contains("note"));
    }

    #[test]
    fn renders_heading() {
        let theme = Theme::default();
        let lines = render("# Title\n\nbody\n", 80, &theme);
        let text = plain(&lines).join("\n");
        assert!(text.contains("Title"));
        assert!(!text.contains("# Title"));
    }

    #[test]
    fn code_comments_are_styled_distinctly() {
        let theme = Theme::default();
        let md = "```rust\n// note\nlet x = 1; // trail\n```\n";
        let lines = render(md, 80, &theme);
        let comment = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("note"))
            .expect("comment span");
        assert!(comment.style.add_modifier.contains(Modifier::ITALIC));
        let code = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("let x"))
            .expect("code span");
        assert!(!code.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn bold_applies_modifier() {
        let theme = Theme::default();
        let lines = render("say **bold** please", 80, &theme);
        let bold = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "bold")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }
}
