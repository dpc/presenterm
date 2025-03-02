use crate::{
    markdown::{
        elements::{
            Code, ListItem, ListItemType, MarkdownElement, ParagraphElement, StyledText, Table, TableRow, Text,
        },
        text::{WeightedLine, WeightedText},
    },
    presentation::{
        AsRenderOperations, MarginProperties, PreformattedLine, Presentation, PresentationMetadata,
        PresentationThemeMetadata, RenderOperation, Slide,
    },
    render::{
        highlighting::{CodeHighlighter, CodeLine},
        properties::WindowSize,
    },
    resource::{LoadImageError, Resources},
    style::{Colors, TextStyle},
    theme::{Alignment, AuthorPositioning, ElementType, FooterStyle, LoadThemeError, Margin, PresentationTheme},
};
use serde::Deserialize;
use std::{borrow::Cow, cell::RefCell, iter, mem, path::PathBuf, rc::Rc, str::FromStr};
use unicode_width::UnicodeWidthStr;

// TODO: move to a theme config.
static DEFAULT_BOTTOM_SLIDE_MARGIN: u16 = 3;

/// Builds a presentation.
///
/// This type transforms [MarkdownElement]s and turns them into a presentation, which is made up of
/// render operations.
pub struct PresentationBuilder<'a> {
    slide_operations: Vec<RenderOperation>,
    slides: Vec<Slide>,
    highlighter: CodeHighlighter,
    theme: Cow<'a, PresentationTheme>,
    resources: &'a mut Resources,
    ignore_element_line_break: bool,
    needs_enter_column: bool,
    last_element_is_list: bool,
    footer_context: Rc<RefCell<FooterContext>>,
    layout: LayoutState,
}

impl<'a> PresentationBuilder<'a> {
    /// Construct a new builder.
    pub fn new(
        default_highlighter: CodeHighlighter,
        default_theme: &'a PresentationTheme,
        resources: &'a mut Resources,
    ) -> Self {
        Self {
            slide_operations: Vec::new(),
            slides: Vec::new(),
            highlighter: default_highlighter,
            theme: Cow::Borrowed(default_theme),
            resources,
            ignore_element_line_break: false,
            last_element_is_list: false,
            needs_enter_column: false,
            footer_context: Default::default(),
            layout: Default::default(),
        }
    }

    /// Build a presentation.
    pub fn build(mut self, elements: Vec<MarkdownElement>) -> Result<Presentation, BuildError> {
        if let Some(MarkdownElement::FrontMatter(contents)) = elements.first() {
            self.process_front_matter(contents)?;
        }
        self.set_code_theme()?;

        if self.slide_operations.is_empty() {
            self.push_slide_prelude();
        }
        for element in elements {
            self.ignore_element_line_break = false;
            self.process_element(element)?;
            self.validate_last_operation()?;
            if !self.ignore_element_line_break {
                self.push_line_break();
            }
        }
        if !self.slide_operations.is_empty() {
            self.terminate_slide(TerminateMode::ResetState);
        }
        self.footer_context.borrow_mut().total_slides = self.slides.len();

        let presentation = Presentation::new(self.slides);
        Ok(presentation)
    }

    fn validate_last_operation(&mut self) -> Result<(), BuildError> {
        if !self.needs_enter_column {
            return Ok(());
        }
        let Some(last) = self.slide_operations.last() else {
            return Ok(());
        };
        if matches!(last, RenderOperation::InitColumnLayout { .. }) {
            return Ok(());
        }
        self.needs_enter_column = false;
        let last_valid = matches!(last, RenderOperation::EnterColumn { .. } | RenderOperation::ExitLayout);
        if last_valid {
            Ok(())
        } else {
            Err(BuildError::NotInsideColumn)
        }
    }

    fn push_slide_prelude(&mut self) {
        let colors = self.theme.default_style.colors.clone();
        self.slide_operations.extend([
            RenderOperation::SetColors(colors),
            RenderOperation::ClearScreen,
            RenderOperation::ApplyMargin(MarginProperties {
                horizontal_margin: self.theme.default_style.margin.clone().unwrap_or_default(),
                bottom_slide_margin: DEFAULT_BOTTOM_SLIDE_MARGIN,
            }),
        ]);
        self.push_line_break();
    }

    fn process_element(&mut self, element: MarkdownElement) -> Result<(), BuildError> {
        let is_list = matches!(element, MarkdownElement::List(_));
        match element {
            // This one is processed before everything else as it affects how the rest of the
            // elements is rendered.
            MarkdownElement::FrontMatter(_) => self.ignore_element_line_break = true,
            MarkdownElement::SetexHeading { text } => self.push_slide_title(text),
            MarkdownElement::Heading { level, text } => self.push_heading(level, text),
            MarkdownElement::Paragraph(elements) => self.push_paragraph(elements)?,
            MarkdownElement::List(elements) => self.push_list(elements),
            MarkdownElement::Code(code) => self.push_code(code),
            MarkdownElement::Table(table) => self.push_table(table),
            MarkdownElement::ThematicBreak => self.push_separator(),
            MarkdownElement::Comment(comment) => self.process_comment(comment)?,
            MarkdownElement::BlockQuote(lines) => self.push_block_quote(lines),
            MarkdownElement::Image(path) => self.push_image(path)?,
        };
        self.last_element_is_list = is_list;
        Ok(())
    }

    fn process_front_matter(&mut self, contents: &str) -> Result<(), BuildError> {
        let metadata: PresentationMetadata =
            serde_yaml::from_str(contents).map_err(|e| BuildError::InvalidMetadata(e.to_string()))?;

        self.footer_context.borrow_mut().author = metadata.author.clone().unwrap_or_default();
        self.set_theme(&metadata.theme)?;
        if metadata.title.is_some() || metadata.sub_title.is_some() || metadata.author.is_some() {
            self.push_slide_prelude();
            self.push_intro_slide(metadata);
        }
        Ok(())
    }

    fn set_theme(&mut self, metadata: &PresentationThemeMetadata) -> Result<(), BuildError> {
        if metadata.name.is_some() && metadata.path.is_some() {
            return Err(BuildError::InvalidMetadata("cannot have both theme path and theme name".into()));
        }
        if let Some(theme_name) = &metadata.name {
            let theme = PresentationTheme::from_name(theme_name)
                .ok_or_else(|| BuildError::InvalidMetadata(format!("theme '{theme_name}' does not exist")))?;
            self.theme = Cow::Owned(theme);
        }
        if let Some(theme_path) = &metadata.path {
            let theme = self.resources.theme(theme_path)?;
            self.theme = Cow::Owned(theme);
        }
        if let Some(overrides) = &metadata.overrides {
            // This shouldn't fail as the models are already correct.
            let theme = merge_struct::merge(self.theme.as_ref(), overrides)
                .map_err(|e| BuildError::InvalidMetadata(format!("invalid theme: {e}")))?;
            self.theme = Cow::Owned(theme);
        }
        Ok(())
    }

    fn set_code_theme(&mut self) -> Result<(), BuildError> {
        if let Some(theme) = &self.theme.code.theme_name {
            let highlighter = CodeHighlighter::new(theme).map_err(|_| BuildError::InvalidCodeTheme)?;
            self.highlighter = highlighter;
        }
        Ok(())
    }

    fn push_intro_slide(&mut self, metadata: PresentationMetadata) {
        let styles = &self.theme.intro_slide;
        let title = StyledText::new(
            metadata.title.unwrap_or_default().clone(),
            TextStyle::default().bold().colors(styles.title.colors.clone()),
        );
        let sub_title = metadata
            .sub_title
            .as_ref()
            .map(|text| StyledText::new(text.clone(), TextStyle::default().colors(styles.subtitle.colors.clone())));
        let author = metadata
            .author
            .as_ref()
            .map(|text| StyledText::new(text.clone(), TextStyle::default().colors(styles.author.colors.clone())));
        self.slide_operations.push(RenderOperation::JumpToVerticalCenter);
        self.push_text(Text::from(title), ElementType::PresentationTitle);
        self.push_line_break();
        if let Some(text) = sub_title {
            self.push_text(Text::from(text), ElementType::PresentationSubTitle);
            self.push_line_break();
        }
        if let Some(text) = author {
            match self.theme.intro_slide.author.positioning {
                AuthorPositioning::BelowTitle => {
                    self.push_line_break();
                    self.push_line_break();
                    self.push_line_break();
                }
                AuthorPositioning::PageBottom => {
                    self.slide_operations.push(RenderOperation::JumpToBottom);
                }
            };
            self.push_text(Text::from(text), ElementType::PresentationAuthor);
        }
        self.terminate_slide(TerminateMode::ResetState);
    }

    fn process_comment(&mut self, comment: String) -> Result<(), BuildError> {
        // Ignore any multi line comment; those are assumed to be user comments
        if comment.contains('\n') {
            return Ok(());
        }
        let comment = comment.parse::<CommentCommand>()?;
        match comment {
            CommentCommand::Pause => self.process_pause(),
            CommentCommand::EndSlide => self.terminate_slide(TerminateMode::ResetState),
            CommentCommand::InitColumnLayout(columns) => {
                Self::validate_column_layout(&columns)?;
                self.layout = LayoutState::InLayout { columns_count: columns.len() };
                self.slide_operations.push(RenderOperation::InitColumnLayout { columns });
                self.needs_enter_column = true;
            }
            CommentCommand::ResetLayout => {
                self.layout = LayoutState::Default;
                self.slide_operations.extend([RenderOperation::ExitLayout, RenderOperation::RenderLineBreak]);
            }
            CommentCommand::Column(column) => {
                let (current_column, columns_count) = match self.layout {
                    LayoutState::InColumn { column, columns_count } => (Some(column), columns_count),
                    LayoutState::InLayout { columns_count } => (None, columns_count),
                    LayoutState::Default => return Err(BuildError::NoLayout),
                };
                if current_column == Some(column) {
                    return Err(BuildError::AlreadyInColumn);
                } else if column >= columns_count {
                    return Err(BuildError::ColumnIndexTooLarge);
                }
                self.layout = LayoutState::InColumn { column, columns_count };
                self.slide_operations.push(RenderOperation::EnterColumn { column });
            }
        };
        // Don't push line breaks for any comments.
        self.ignore_element_line_break = true;
        Ok(())
    }

    fn validate_column_layout(columns: &[u8]) -> Result<(), BuildError> {
        if columns.is_empty() {
            Err(BuildError::InvalidLayout("need at least one column"))
        } else if columns.iter().any(|column| column == &0) {
            Err(BuildError::InvalidLayout("can't have zero sized columns"))
        } else {
            Ok(())
        }
    }

    fn process_pause(&mut self) {
        // Remove the last line, if any, if the previous element is a list. This allows each
        // element in a list showing up without newlines in between..
        if self.last_element_is_list && matches!(self.slide_operations.last(), Some(RenderOperation::RenderLineBreak)) {
            self.slide_operations.pop();
        }

        let next_operations = self.slide_operations.clone();
        self.terminate_slide(TerminateMode::KeepState);
        self.slide_operations = next_operations;
    }

    fn push_slide_title(&mut self, mut text: Text) {
        let style = self.theme.slide_title.clone();
        text.apply_style(&TextStyle::default().bold().colors(style.colors.clone()));

        for _ in 0..style.padding_top.unwrap_or(0) {
            self.push_line_break();
        }
        self.push_text(text, ElementType::SlideTitle);
        self.push_line_break();

        for _ in 0..style.padding_bottom.unwrap_or(0) {
            self.push_line_break();
        }
        if style.separator {
            self.slide_operations.push(RenderOperation::RenderSeparator);
        }
        self.push_line_break();
        self.ignore_element_line_break = true;
    }

    fn push_heading(&mut self, level: u8, mut text: Text) {
        let (element_type, style) = match level {
            1 => (ElementType::Heading1, &self.theme.headings.h1),
            2 => (ElementType::Heading2, &self.theme.headings.h2),
            3 => (ElementType::Heading3, &self.theme.headings.h3),
            4 => (ElementType::Heading4, &self.theme.headings.h4),
            5 => (ElementType::Heading5, &self.theme.headings.h5),
            6 => (ElementType::Heading6, &self.theme.headings.h6),
            other => panic!("unexpected heading level {other}"),
        };
        if let Some(prefix) = &style.prefix {
            let mut prefix = prefix.clone();
            prefix.push(' ');
            text.chunks.insert(0, StyledText::from(prefix));
        }
        let text_style = TextStyle::default().bold().colors(style.colors.clone());
        text.apply_style(&text_style);

        self.push_text(text, element_type);
        self.push_line_break();
    }

    fn push_paragraph(&mut self, elements: Vec<ParagraphElement>) -> Result<(), BuildError> {
        for element in elements {
            match element {
                ParagraphElement::Text(text) => {
                    self.push_text(text, ElementType::Paragraph);
                    self.push_line_break();
                }
                ParagraphElement::LineBreak => {
                    // Line breaks are already pushed after every text chunk.
                }
            };
        }
        Ok(())
    }

    fn push_separator(&mut self) {
        self.slide_operations.extend([RenderOperation::RenderSeparator, RenderOperation::RenderLineBreak]);
    }

    fn push_image(&mut self, path: PathBuf) -> Result<(), BuildError> {
        let image = self.resources.image(&path)?;
        self.slide_operations.push(RenderOperation::RenderImage(image));
        Ok(())
    }

    fn push_list(&mut self, items: Vec<ListItem>) {
        for item in items {
            self.push_list_item(item);
        }
    }

    fn push_list_item(&mut self, item: ListItem) {
        let padding_length = (item.depth as usize + 1) * 3;
        let mut prefix: String = " ".repeat(padding_length);
        match item.item_type {
            ListItemType::Unordered => {
                let delimiter = match item.depth {
                    0 => '•',
                    1 => '◦',
                    _ => '▪',
                };
                prefix.push(delimiter);
            }
            ListItemType::OrderedParens(number) => {
                prefix.push_str(&number.to_string());
                prefix.push_str(") ");
            }
            ListItemType::OrderedPeriod(number) => {
                prefix.push_str(&number.to_string());
                prefix.push_str(". ");
            }
        };

        let prefix_length = prefix.len() as u16;
        self.push_text(prefix.into(), ElementType::List);

        let text = item.contents;
        self.push_aligned_text(text, Alignment::Left { margin: Margin::Fixed(prefix_length) });
        self.push_line_break();
    }

    fn push_block_quote(&mut self, lines: Vec<String>) {
        let prefix = self.theme.block_quote.prefix.clone().unwrap_or_default();
        let block_length = lines.iter().map(|line| line.width() + prefix.width()).max().unwrap_or(0);

        self.slide_operations.push(RenderOperation::SetColors(self.theme.block_quote.colors.clone()));
        for mut line in lines {
            line.insert_str(0, &prefix);

            let line_length = line.width();
            self.slide_operations.push(RenderOperation::RenderPreformattedLine(PreformattedLine {
                text: line,
                unformatted_length: line_length,
                block_length,
                alignment: self.theme.alignment(&ElementType::BlockQuote).clone(),
            }));
            self.push_line_break();
        }
        self.slide_operations.push(RenderOperation::SetColors(self.theme.default_style.colors.clone()));
    }

    fn push_text(&mut self, text: Text, element_type: ElementType) {
        let alignment = self.theme.alignment(&element_type);
        self.push_aligned_text(text, alignment);
    }

    fn push_aligned_text(&mut self, text: Text, alignment: Alignment) {
        let mut texts: Vec<WeightedText> = Vec::new();
        for mut chunk in text.chunks {
            if chunk.style.is_code() {
                chunk.style.colors = self.theme.inline_code.colors.clone();
            }
            texts.push(chunk.into());
        }
        if !texts.is_empty() {
            self.slide_operations.push(RenderOperation::RenderTextLine {
                line: WeightedLine::from(texts),
                alignment: alignment.clone(),
            });
        }
    }

    fn push_line_break(&mut self) {
        self.slide_operations.push(RenderOperation::RenderLineBreak);
    }

    fn push_code(&mut self, code: Code) {
        let Code { contents, language } = code;
        let mut code = String::new();
        let horizontal_padding = self.theme.code.padding.horizontal.unwrap_or(0);
        let vertical_padding = self.theme.code.padding.vertical.unwrap_or(0);
        if horizontal_padding == 0 && vertical_padding == 0 {
            code = contents;
        } else {
            if vertical_padding > 0 {
                code.push('\n');
            }
            if horizontal_padding > 0 {
                let padding = " ".repeat(horizontal_padding as usize);
                for line in contents.lines() {
                    code.push_str(&padding);
                    code.push_str(line);
                    code.push('\n');
                }
            } else {
                code.push_str(&contents);
            }
            if vertical_padding > 0 {
                code.push('\n');
            }
        }
        let block_length = code.lines().map(|line| line.width()).max().unwrap_or(0) + horizontal_padding as usize;
        for code_line in self.highlighter.highlight(&code, &language) {
            let CodeLine { formatted, original } = code_line;
            let trimmed = formatted.trim_end();
            let original_length = original.width() - (formatted.width() - trimmed.width());
            self.slide_operations.push(RenderOperation::RenderPreformattedLine(PreformattedLine {
                text: trimmed.into(),
                unformatted_length: original_length,
                block_length,
                alignment: self.theme.alignment(&ElementType::Code),
            }));
            self.push_line_break();
        }
    }

    fn terminate_slide(&mut self, mode: TerminateMode) {
        self.push_footer();

        let elements = mem::take(&mut self.slide_operations);
        self.slides.push(Slide { render_operations: elements });
        self.push_slide_prelude();
        if matches!(mode, TerminateMode::ResetState) {
            self.ignore_element_line_break = true;
            self.needs_enter_column = false;
            self.layout = Default::default();
        }
    }

    fn push_footer(&mut self) {
        let generator = FooterGenerator {
            style: self.theme.footer.clone(),
            current_slide: self.slides.len(),
            context: self.footer_context.clone(),
        };
        self.slide_operations.extend([
            // Exit any layout we're in so this gets rendered on a default screen size.
            RenderOperation::ExitLayout,
            // Pop the slide margin so we're at the terminal rect.
            RenderOperation::PopMargin,
            // Jump to the very bottom of the terminal rect and draw the footer.
            RenderOperation::JumpToBottom,
            RenderOperation::RenderDynamic(Rc::new(generator)),
        ]);
    }

    fn push_table(&mut self, table: Table) {
        let widths: Vec<_> = (0..table.columns())
            .map(|column| table.iter_column(column).map(|text| text.width()).max().unwrap_or(0))
            .collect();
        let flattened_header = Self::prepare_table_row(table.header, &widths);
        self.push_text(flattened_header, ElementType::Table);
        self.push_line_break();

        let mut separator = Text { chunks: Vec::new() };
        for (index, width) in widths.iter().enumerate() {
            let mut contents = String::new();
            let mut margin = 1;
            if index > 0 {
                contents.push('┼');
                // Append an extra dash to have 1 column margin on both sides
                if index < widths.len() - 1 {
                    margin += 1;
                }
            }
            contents.extend(iter::repeat("─").take(*width + margin));
            separator.chunks.push(StyledText::from(contents));
        }

        self.push_text(separator, ElementType::Table);
        self.push_line_break();

        for row in table.rows {
            let flattened_row = Self::prepare_table_row(row, &widths);
            self.push_text(flattened_row, ElementType::Table);
            self.push_line_break();
        }
    }

    fn prepare_table_row(row: TableRow, widths: &[usize]) -> Text {
        let mut flattened_row = Text { chunks: Vec::new() };
        for (column, text) in row.0.into_iter().enumerate() {
            if column > 0 {
                flattened_row.chunks.push(StyledText::from(" │ "));
            }
            let text_length = text.width();
            flattened_row.chunks.extend(text.chunks.into_iter());

            let cell_width = widths[column];
            if text_length < cell_width {
                let padding = " ".repeat(cell_width - text_length);
                flattened_row.chunks.push(StyledText::from(padding));
            }
        }
        flattened_row
    }
}

enum TerminateMode {
    KeepState,
    ResetState,
}

#[derive(Debug, Default)]
enum LayoutState {
    #[default]
    Default,
    InLayout {
        columns_count: usize,
    },
    InColumn {
        column: usize,
        columns_count: usize,
    },
}

#[derive(Debug, Default)]
struct FooterContext {
    total_slides: usize,
    author: String,
}

#[derive(Debug)]
struct FooterGenerator {
    current_slide: usize,
    context: Rc<RefCell<FooterContext>>,
    style: FooterStyle,
}

impl FooterGenerator {
    fn render_template(
        template: &str,
        current_slide: &str,
        context: &FooterContext,
        colors: Colors,
        alignment: Alignment,
    ) -> RenderOperation {
        let contents = template
            .replace("{current_slide}", current_slide)
            .replace("{total_slides}", &context.total_slides.to_string())
            .replace("{author}", &context.author);
        let text = WeightedText::from(StyledText::new(contents, TextStyle::default().colors(colors)));
        RenderOperation::RenderTextLine { line: vec![text].into(), alignment }
    }
}

impl AsRenderOperations for FooterGenerator {
    fn as_render_operations(&self, dimensions: &WindowSize) -> Vec<RenderOperation> {
        let context = self.context.borrow();
        match &self.style {
            FooterStyle::Template { left, center, right, colors } => {
                let current_slide = (self.current_slide + 1).to_string();
                let mut operations = Vec::new();
                let margin = Margin::Fixed(1);
                let alignments = [
                    Alignment::Left { margin: margin.clone() },
                    Alignment::Center { minimum_size: 0, minimum_margin: margin.clone() },
                    Alignment::Right { margin: margin.clone() },
                ];
                for (text, alignment) in [left, center, right].iter().zip(alignments) {
                    if let Some(text) = text {
                        operations.push(Self::render_template(
                            text,
                            &current_slide,
                            &context,
                            colors.clone(),
                            alignment,
                        ));
                    }
                }
                operations
            }
            FooterStyle::ProgressBar { character, colors } => {
                let character = character.unwrap_or('█').to_string();
                let total_columns = dimensions.columns as usize / character.width();
                let progress_ratio = (self.current_slide + 1) as f64 / context.total_slides as f64;
                let columns_ratio = (total_columns as f64 * progress_ratio).ceil();
                let bar = character.repeat(columns_ratio as usize);
                let bar = vec![WeightedText::from(StyledText::new(bar, TextStyle::default().colors(colors.clone())))];
                vec![RenderOperation::RenderTextLine {
                    line: bar.into(),
                    alignment: Alignment::Left { margin: Margin::Fixed(0) },
                }]
            }
            FooterStyle::Empty => vec![],
        }
    }
}

/// An error when building a presentation.
#[derive(thiserror::Error, Debug)]
pub enum BuildError {
    #[error("loading image: {0}")]
    LoadImage(#[from] LoadImageError),

    #[error("invalid presentation metadata: {0}")]
    InvalidMetadata(String),

    #[error("invalid theme: {0}")]
    InvalidTheme(#[from] LoadThemeError),

    #[error("invalid code highlighter theme")]
    InvalidCodeTheme,

    #[error("invalid layout: {0}")]
    InvalidLayout(&'static str),

    #[error("can't enter layout: no layout defined")]
    NoLayout,

    #[error("can't enter layout column: already in it")]
    AlreadyInColumn,

    #[error("can't enter layout column: column index too large")]
    ColumnIndexTooLarge,

    #[error("need to enter layout column explicitly using `column` command")]
    NotInsideColumn,

    #[error(transparent)]
    CommandParse(#[from] CommandParseError),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CommentCommand {
    Pause,
    EndSlide,
    #[serde(rename = "column_layout")]
    InitColumnLayout(Vec<u8>),
    Column(usize),
    ResetLayout,
}

impl FromStr for CommentCommand {
    type Err = CommandParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        #[derive(Deserialize)]
        struct CommandWrapper(#[serde(with = "serde_yaml::with::singleton_map")] CommentCommand);

        let wrapper = serde_yaml::from_str::<CommandWrapper>(s)?;
        Ok(wrapper.0)
    }
}

#[derive(thiserror::Error, Debug)]
#[error("invalid command: {0}")]
pub struct CommandParseError(#[from] serde_yaml::Error);

#[cfg(test)]
mod test {
    use rstest::rstest;

    use super::*;
    use crate::{markdown::elements::ProgrammingLanguage, presentation::PreformattedLine};

    fn build_presentation(elements: Vec<MarkdownElement>) -> Presentation {
        try_build_presentation(elements).expect("build failed")
    }

    fn try_build_presentation(elements: Vec<MarkdownElement>) -> Result<Presentation, BuildError> {
        let highlighter = CodeHighlighter::new("base16-ocean.dark").unwrap();
        let theme = PresentationTheme::default();
        let mut resources = Resources::new("/tmp");
        let builder = PresentationBuilder::new(highlighter, &theme, &mut resources);
        builder.build(elements)
    }

    fn build_pause() -> MarkdownElement {
        MarkdownElement::Comment("pause".into())
    }

    fn build_end_slide() -> MarkdownElement {
        MarkdownElement::Comment("end_slide".into())
    }

    fn build_column_layout(width: u8) -> MarkdownElement {
        MarkdownElement::Comment(format!("column_layout: [{width}]"))
    }

    fn build_column(column: u8) -> MarkdownElement {
        MarkdownElement::Comment(format!("column: {column}"))
    }

    fn is_visible(operation: &RenderOperation) -> bool {
        use RenderOperation::*;
        match operation {
            ClearScreen
            | SetColors(_)
            | JumpToVerticalCenter
            | JumpToBottom
            | InitColumnLayout { .. }
            | EnterColumn { .. }
            | ExitLayout { .. }
            | ApplyMargin(_)
            | PopMargin => false,
            RenderTextLine { .. }
            | RenderSeparator
            | RenderLineBreak
            | RenderImage(_)
            | RenderPreformattedLine(_)
            | RenderDynamic(_) => true,
        }
    }

    fn extract_text_lines(operations: &[RenderOperation]) -> Vec<String> {
        let mut output = Vec::new();
        for operation in operations {
            match operation {
                RenderOperation::RenderTextLine { line, .. } => {
                    let texts: Vec<_> = line.iter_texts().map(|text| text.text.text.clone()).collect();
                    output.push(texts.join(""));
                }
                _ => (),
            };
        }
        output
    }

    #[test]
    fn prelude_appears_once() {
        let elements = vec![
            MarkdownElement::FrontMatter("author: bob".to_string()),
            MarkdownElement::Heading { text: Text::from("hello"), level: 1 },
            MarkdownElement::Comment("end_slide".to_string()),
            MarkdownElement::Heading { text: Text::from("bye"), level: 1 },
        ];
        let presentation = build_presentation(elements);
        for (index, slide) in presentation.iter_slides().into_iter().enumerate() {
            let clear_screen_count =
                slide.render_operations.iter().filter(|op| matches!(op, RenderOperation::ClearScreen)).count();
            let set_colors_count =
                slide.render_operations.iter().filter(|op| matches!(op, RenderOperation::SetColors(_))).count();
            assert_eq!(clear_screen_count, 1, "{clear_screen_count} clear screens in slide {index}");
            assert_eq!(set_colors_count, 1, "{set_colors_count} clear screens in slide {index}");
        }
    }

    #[test]
    fn slides_start_with_one_newline() {
        let elements = vec![
            MarkdownElement::FrontMatter("author: bob".to_string()),
            MarkdownElement::Heading { text: Text::from("hello"), level: 1 },
            MarkdownElement::Comment("end_slide".to_string()),
            MarkdownElement::Heading { text: Text::from("bye"), level: 1 },
        ];
        let presentation = build_presentation(elements);
        assert_eq!(presentation.iter_slides().count(), 3);

        // Don't process the intro slide as it's special
        let slides = presentation.into_slides().into_iter().skip(1);
        for slide in slides {
            let mut ops = slide.render_operations.into_iter().filter(is_visible);
            // We should start with a newline
            assert!(matches!(ops.next(), Some(RenderOperation::RenderLineBreak)));
            // And the second one should _not_ be a newline
            assert!(!matches!(ops.next(), Some(RenderOperation::RenderLineBreak)));
        }
    }

    #[test]
    fn preformatted_blocks_account_for_unicode_widths() {
        let text = "苹果".to_string();
        let elements = vec![
            MarkdownElement::BlockQuote(vec![text.clone()]),
            MarkdownElement::Code(Code { contents: text.clone(), language: ProgrammingLanguage::Unknown }),
        ];
        let presentation = build_presentation(elements);
        let slides = presentation.into_slides();
        let lengths: Vec<_> = slides[0]
            .render_operations
            .iter()
            .filter_map(|op| match op {
                RenderOperation::RenderPreformattedLine(PreformattedLine {
                    block_length, unformatted_length, ..
                }) => Some((block_length, unformatted_length)),
                _ => None,
            })
            .collect();
        assert_eq!(lengths.len(), 2);
        let width = &text.width();
        assert_eq!(lengths[0], (width, width));
        assert_eq!(lengths[1], (width, width));
    }

    #[test]
    fn table() {
        let elements = vec![MarkdownElement::Table(Table {
            header: TableRow(vec![Text::from("key"), Text::from("value"), Text::from("other")]),
            rows: vec![TableRow(vec![Text::from("potato"), Text::from("bar"), Text::from("yes")])],
        })];
        let slides = build_presentation(elements).into_slides();
        let operations: Vec<_> =
            slides.into_iter().next().unwrap().render_operations.into_iter().filter(|op| is_visible(op)).collect();
        let lines = extract_text_lines(&operations);
        let expected_lines = &["key    │ value │ other", "───────┼───────┼──────", "potato │ bar   │ yes  "];
        assert_eq!(lines, expected_lines);
    }

    #[test]
    fn layout_without_init() {
        let elements = vec![MarkdownElement::Comment("column: 0".into())];
        let result = try_build_presentation(elements);
        assert!(result.is_err());
    }

    #[test]
    fn already_in_column() {
        let elements = vec![
            MarkdownElement::Comment("column_layout: [1]".into()),
            MarkdownElement::Comment("column: 0".into()),
            MarkdownElement::Comment("column: 0".into()),
        ];
        let result = try_build_presentation(elements);
        assert!(result.is_err());
    }

    #[test]
    fn column_index_overflow() {
        let elements =
            vec![MarkdownElement::Comment("column_layout: [1]".into()), MarkdownElement::Comment("column: 1".into())];
        let result = try_build_presentation(elements);
        assert!(result.is_err());
    }

    #[rstest]
    #[case::empty("column_layout: []")]
    #[case::zero("column_layout: [0]")]
    #[case::one_is_zero("column_layout: [1, 0]")]
    fn invalid_layouts(#[case] definition: &str) {
        let elements = vec![MarkdownElement::Comment(definition.into())];
        let result = try_build_presentation(elements);
        assert!(result.is_err());
    }

    #[test]
    fn operation_without_enter_column() {
        let elements = vec![MarkdownElement::Comment("column_layout: [1]".into()), MarkdownElement::ThematicBreak];
        let result = try_build_presentation(elements);
        assert!(result.is_err());
    }

    #[rstest]
    #[case::pause("pause", CommentCommand::Pause)]
    #[case::pause(" pause ", CommentCommand::Pause)]
    #[case::end_slide("end_slide", CommentCommand::EndSlide)]
    #[case::column_layout("column_layout: [1, 2]", CommentCommand::InitColumnLayout(vec![1, 2]))]
    #[case::column("column: 1", CommentCommand::Column(1))]
    #[case::reset_layout("reset_layout", CommentCommand::ResetLayout)]
    fn command_formatting(#[case] input: &str, #[case] expected: CommentCommand) {
        let parsed: CommentCommand = input.parse().expect("deserialization failed");
        assert_eq!(parsed, expected);
    }

    #[test]
    fn end_slide_inside_layout() {
        let elements = vec![build_column_layout(1), build_end_slide()];
        let presentation = build_presentation(elements);
        assert_eq!(presentation.iter_slides().count(), 2);
    }

    #[test]
    fn end_slide_inside_column() {
        let elements = vec![build_column_layout(1), build_column(0), build_end_slide()];
        let presentation = build_presentation(elements);
        assert_eq!(presentation.iter_slides().count(), 2);
    }

    #[test]
    fn pause_inside_layout() {
        let elements = vec![build_column_layout(1), build_pause(), build_column(0)];
        let presentation = build_presentation(elements);
        assert_eq!(presentation.iter_slides().count(), 2);
    }
}
