use roxmltree::{Document, Node};

use crate::core::model::{Annotation, AnnotationKind, LineRole, SyncedTrack};
use crate::core::normalize::normalize_track_checked;
use crate::core::parse::{ParsedLine, ParsedTrack, ParsedWord, trim_line_with_words};
use crate::providers::ProviderError;

pub const MAX_XML_DEPTH: usize = 32;

const TTM_NAMESPACE: &str = "http://www.w3.org/ns/ttml#metadata";
const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

#[derive(Clone, Copy)]
struct Timing {
    start: u64,
    end: Option<u64>,
}

#[derive(Clone, Copy)]
enum Destination {
    Main,
    Background,
}

#[derive(Default)]
struct Builder {
    text: String,
    words: Vec<ParsedWord>,
}

#[derive(Default)]
struct LineContent {
    main: Builder,
    background: Builder,
    annotations: Vec<Annotation>,
}

pub fn parse_ttml(xml_str: &str, source: &str) -> Result<SyncedTrack, ProviderError> {
    let document = Document::parse(xml_str)
        .map_err(|error| ProviderError::Invalid(format!("invalid TTML XML: {error}")))?;
    validate_depth(&document)?;

    let primary_agent = find_primary_agent(&document);
    let mut parsed = ParsedTrack::default();

    for node in document.descendants().filter(|node| {
        node.is_element()
            && node.tag_name().name() == "p"
            && !node
                .ancestors()
                .skip(1)
                .any(|ancestor| ancestor.tag_name().name() == "p")
    }) {
        if let Some(line) = parse_line(node, primary_agent.as_deref())? {
            parsed.lines.push(line);
        }
    }

    parsed.lines.sort_by_key(|line| line.start_ms);

    if parsed.lines.is_empty() {
        return Err(ProviderError::Invalid(
            "TTML document contains no lyric lines".to_string(),
        ));
    }

    let track = normalize_track_checked(parsed, source, None, 1.0, None, 0)
        .map_err(|error| ProviderError::Invalid(error.to_string()))?;

    if track.lines.is_empty() {
        return Err(ProviderError::Invalid(
            "TTML document contains no lyric lines".to_string(),
        ));
    }

    Ok(track)
}

fn validate_depth(document: &Document<'_>) -> Result<(), ProviderError> {
    for node in document.descendants().filter(|node| node.is_element()) {
        let depth = node.ancestors().count().saturating_sub(1);
        if depth > MAX_XML_DEPTH {
            return Err(ProviderError::Invalid(format!(
                "TTML XML depth exceeds {MAX_XML_DEPTH}"
            )));
        }
    }

    Ok(())
}

fn find_primary_agent(document: &Document<'_>) -> Option<String> {
    for node in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "agent")
    {
        if let Some(id) = id_attribute(node) {
            let id = trim_agent_id(id);
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }

    document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "p")
        .find_map(|node| {
            ttm_attribute(node, "agent")
                .and_then(first_agent_id)
                .map(str::to_string)
        })
}

fn parse_line(
    node: Node<'_, '_>,
    primary_agent: Option<&str>,
) -> Result<Option<ParsedLine>, ProviderError> {
    let Some(timing) = line_timing(node)? else {
        return Ok(None);
    };

    let background_line = role_has(node, "x-bg");
    let agent = ttm_attribute(node, "agent").or_else(|| {
        node.children()
            .find_map(|child| ttm_attribute(child, "agent"))
    });
    let mut role = if background_line {
        LineRole::BackgroundVocal
    } else if agent.is_some_and(|agent| !agent_is_primary(agent, primary_agent)) {
        LineRole::DuetSecondary
    } else {
        LineRole::Primary
    };

    let mut content = LineContent::default();
    collect_children(
        node,
        timing,
        false,
        Destination::Main,
        &mut content,
        background_line,
    )?;

    let LineContent {
        main,
        background,
        mut annotations,
    } = content;

    let main = if main.text.trim().is_empty()
        && main.words.is_empty()
        && !background.text.trim().is_empty()
    {
        role = LineRole::BackgroundVocal;
        background
    } else {
        push_annotation(
            &mut annotations,
            AnnotationKind::Background,
            background.text.trim(),
        );
        main
    };

    let (text, words) = trim_line_with_words(main.text, main.words);
    if text.trim().is_empty() && words.is_empty() {
        return Ok(None);
    }

    Ok(Some(ParsedLine {
        start_ms: timing.start,
        end_ms: timing.end,
        role,
        text,
        words,
        annotations,
        explicit_last_word_end_ms: None,
    }))
}

fn collect_children(
    parent: Node<'_, '_>,
    timing: Timing,
    timed: bool,
    destination: Destination,
    content: &mut LineContent,
    line_is_background: bool,
) -> Result<(), ProviderError> {
    for child in parent.children() {
        if child.is_text() {
            let Some(raw) = child.text() else {
                continue;
            };
            let builder = match destination {
                Destination::Main => &mut content.main,
                Destination::Background => &mut content.background,
            };
            append_text(builder, raw, timed.then_some(timing));
            continue;
        }

        if !child.is_element() {
            continue;
        }

        if child.tag_name().name() == "br" {
            let builder = match destination {
                Destination::Main => &mut content.main,
                Destination::Background => &mut content.background,
            };
            if !builder.text.is_empty() && !builder.text.ends_with(' ') {
                builder.text.push(' ');
            }
            continue;
        }

        if let Some(kind) = annotation_kind(child) {
            if let Some(text) = collect_text(child) {
                push_annotation(&mut content.annotations, kind, &text);
            }
            continue;
        }

        let destination = if role_has(child, "x-bg") && !line_is_background {
            Destination::Background
        } else {
            destination
        };
        let (child_timing, child_timed) = element_timing(child, timing, timed)?;
        collect_children(
            child,
            child_timing,
            child_timed,
            destination,
            content,
            line_is_background,
        )?;
    }

    Ok(())
}

fn append_text(builder: &mut Builder, raw: &str, timing: Option<Timing>) {
    let start = builder.text.len();
    append_normalized(&mut builder.text, raw);
    let end = builder.text.len();

    if start < end
        && let Some(timing) = timing
    {
        builder.words.push(ParsedWord {
            byte_range: (start, end),
            start_ms: timing.start,
            end_ms: timing.end,
        });
    }
}

fn append_normalized(output: &mut String, raw: &str) {
    if raw.trim().is_empty()
        && raw
            .chars()
            .any(|character| matches!(character, '\n' | '\r' | '\t'))
    {
        return;
    }

    let mut last_was_space = output.ends_with(' ');

    for character in raw.chars() {
        if character.is_whitespace() {
            if !last_was_space && !output.is_empty() {
                output.push(' ');
                last_was_space = true;
            }
        } else {
            output.push(character);
            last_was_space = false;
        }
    }
}

fn collect_text(node: Node<'_, '_>) -> Option<String> {
    let mut text = String::new();

    for child in node.descendants().filter(|node| node.is_text()) {
        if let Some(raw) = child.text() {
            append_normalized(&mut text, raw);
        }
    }

    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn push_annotation(annotations: &mut Vec<Annotation>, kind: AnnotationKind, text: &str) {
    let text = text.trim();
    if text.is_empty()
        || annotations
            .iter()
            .any(|annotation| annotation.kind == kind && annotation.text == text)
    {
        return;
    }

    annotations.push(Annotation {
        kind,
        text: text.to_string(),
    });
}

fn annotation_kind(node: Node<'_, '_>) -> Option<AnnotationKind> {
    if role_has(node, "x-translation") {
        Some(AnnotationKind::Translation)
    } else if role_has(node, "x-roman") || role_has(node, "x-romanization") {
        Some(AnnotationKind::Romanization)
    } else {
        None
    }
}

fn line_timing(node: Node<'_, '_>) -> Result<Option<Timing>, ProviderError> {
    let begin = time_attribute(node, "begin")?;
    let start = match begin {
        Some(start) => start,
        None => {
            let mut earliest = None;
            for child in node.descendants().filter(|n| n.is_element()) {
                if let Some(b) = time_attribute(child, "begin")? {
                    earliest = Some(earliest.map_or(b, |e: u64| e.min(b)));
                }
            }
            match earliest {
                Some(start) => start,
                None => return Ok(None),
            }
        }
    };

    let end = match (time_attribute(node, "end")?, time_attribute(node, "dur")?) {
        (Some(end), _) => Some(end),
        (None, Some(duration)) => Some(add_duration(start, duration)?),
        (None, None) => {
            let mut latest = None;
            for child in node.descendants().filter(|n| n.is_element()) {
                if let Some(e) = time_attribute(child, "end")? {
                    latest = Some(latest.map_or(e, |l: u64| l.max(e)));
                } else if let (Some(b), Some(d)) = (
                    time_attribute(child, "begin")?,
                    time_attribute(child, "dur")?,
                ) {
                    let end_time = add_duration(b, d)?;
                    latest = Some(latest.map_or(end_time, |l: u64| l.max(end_time)));
                }
            }
            latest
        }
    };

    if end.is_some_and(|end| end < start) {
        return Err(ProviderError::Invalid(
            "TTML line ends before it starts".to_string(),
        ));
    }

    Ok(Some(Timing { start, end }))
}

fn element_timing(
    node: Node<'_, '_>,
    parent: Timing,
    parent_timed: bool,
) -> Result<(Timing, bool), ProviderError> {
    let begin = time_attribute(node, "begin")?;
    let end = time_attribute(node, "end")?;
    let duration = time_attribute(node, "dur")?;
    let own_timing = begin.is_some() || end.is_some() || duration.is_some();
    let start = begin.unwrap_or(parent.start);
    let end = match (end, duration) {
        (Some(end), _) => Some(end),
        (None, Some(duration)) => Some(add_duration(start, duration)?),
        (None, None) if begin.is_some() => None,
        (None, None) => parent.end,
    };

    if end.is_some_and(|end| end < start) {
        return Err(ProviderError::Invalid(
            "TTML element ends before it starts".to_string(),
        ));
    }

    Ok((Timing { start, end }, parent_timed || own_timing))
}

fn time_attribute(node: Node<'_, '_>, name: &str) -> Result<Option<u64>, ProviderError> {
    node.attribute(name).map(parse_timestamp).transpose()
}

fn add_duration(start: u64, duration: u64) -> Result<u64, ProviderError> {
    start
        .checked_add(duration)
        .ok_or_else(|| ProviderError::Invalid("TTML timestamp overflow".to_string()))
}

fn parse_timestamp(value: &str) -> Result<u64, ProviderError> {
    let value = value.trim();
    let value = value.strip_prefix('+').unwrap_or(value);
    let invalid = || ProviderError::Invalid(format!("invalid TTML timestamp {value:?}"));

    if let Some(millis) = value.strip_suffix("ms") {
        let millis = millis.trim().parse::<f64>().map_err(|_| invalid())?;
        if !millis.is_finite() || millis < 0.0 || millis > u64::MAX as f64 {
            return Err(invalid());
        }
        return Ok(millis.round() as u64);
    }

    if let Some(seconds) = value.strip_suffix('s') {
        let seconds = seconds.trim().parse::<f64>().map_err(|_| invalid())?;
        let millis = seconds * 1000.0;
        if !millis.is_finite() || millis < 0.0 || millis > u64::MAX as f64 {
            return Err(invalid());
        }
        return Ok(millis.round() as u64);
    }

    if !value.contains(':') {
        let seconds = value.parse::<f64>().map_err(|_| invalid())?;
        let millis = seconds * 1000.0;
        if !millis.is_finite() || millis < 0.0 || millis > u64::MAX as f64 {
            return Err(invalid());
        }
        return Ok(millis.round() as u64);
    }

    let mut parts = value.split(':');
    let first = parts.next().ok_or_else(invalid)?;
    let second = parts.next().ok_or_else(invalid)?;
    let third = parts.next();
    if parts.next().is_some() {
        return Err(invalid());
    }

    match third {
        None => {
            let minutes = first.parse::<u64>().map_err(|_| invalid())?;
            let (seconds, millis) = parse_clock_seconds(second, true)?;
            minutes
                .checked_mul(60_000)
                .and_then(|value| seconds.checked_mul(1000).and_then(|s| value.checked_add(s)))
                .and_then(|value| value.checked_add(millis))
                .ok_or_else(invalid)
        }
        Some(third) => {
            let hours = first.parse::<u64>().map_err(|_| invalid())?;
            let minutes = second.parse::<u64>().map_err(|_| invalid())?;
            if minutes >= 60 {
                return Err(invalid());
            }
            let (seconds, millis) = parse_clock_seconds(third, true)?;
            hours
                .checked_mul(3_600_000)
                .and_then(|value| {
                    minutes
                        .checked_mul(60_000)
                        .and_then(|m| value.checked_add(m))
                })
                .and_then(|value| seconds.checked_mul(1000).and_then(|s| value.checked_add(s)))
                .and_then(|value| value.checked_add(millis))
                .ok_or_else(invalid)
        }
    }
}

fn parse_clock_seconds(value: &str, enforce_range: bool) -> Result<(u64, u64), ProviderError> {
    let (whole, fraction) = value
        .split_once('.')
        .or_else(|| value.split_once(','))
        .unwrap_or((value, ""));
    if whole.is_empty() {
        return Err(ProviderError::Invalid(format!(
            "invalid TTML timestamp {value:?}"
        )));
    }

    let seconds = whole
        .parse::<u64>()
        .map_err(|_| ProviderError::Invalid(format!("invalid TTML timestamp {value:?}")))?;
    if enforce_range && seconds >= 60 {
        return Err(ProviderError::Invalid(format!(
            "invalid TTML timestamp {value:?}"
        )));
    }

    let millis = if value.contains('.') || value.contains(',') {
        parse_fraction(fraction)?
    } else {
        0
    };

    Ok((seconds, millis))
}

fn parse_fraction(fraction: &str) -> Result<u64, ProviderError> {
    if fraction.is_empty() {
        return Err(ProviderError::Invalid(
            "TTML timestamp has an empty fraction".to_string(),
        ));
    }

    let mut millis = 0u64;
    let mut digits = 0usize;

    for character in fraction.chars() {
        if !character.is_ascii_digit() {
            return Err(ProviderError::Invalid(
                "TTML timestamp fraction is not numeric".to_string(),
            ));
        }
        if digits < 3 {
            millis = millis * 10 + u64::from(character.to_digit(10).unwrap_or(0));
            digits += 1;
        }
    }

    while digits < 3 {
        millis *= 10;
        digits += 1;
    }

    Ok(millis)
}

fn ttm_attribute<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.attribute((TTM_NAMESPACE, name))
        .or_else(|| {
            node.attributes()
                .find(|attr| {
                    attr.name() == name
                        && (attr.namespace() == Some(TTM_NAMESPACE)
                            || attr.namespace().is_none()
                            || attr
                                .namespace()
                                .is_some_and(|ns| ns.contains("ttml") || ns.contains("metadata")))
                })
                .map(|attr| attr.value())
        })
        .or_else(|| node.attribute(name))
}

fn id_attribute<'a>(node: Node<'a, '_>) -> Option<&'a str> {
    node.attribute((XML_NAMESPACE, "id"))
        .or_else(|| {
            node.attributes()
                .find(|attr| attr.name() == "id")
                .map(|attr| attr.value())
        })
        .or_else(|| node.attribute("id"))
}

fn role_has(node: Node<'_, '_>, role: &str) -> bool {
    ttm_attribute(node, "role")
        .is_some_and(|value| value.split_whitespace().any(|candidate| candidate == role))
}

fn first_agent_id(value: &str) -> Option<&str> {
    value
        .split_whitespace()
        .map(trim_agent_id)
        .find(|agent| !agent.is_empty())
}

fn trim_agent_id(value: &str) -> &str {
    value.trim_start_matches('#')
}

fn agent_is_primary(value: &str, primary: Option<&str>) -> bool {
    let Some(primary) = primary else {
        return true;
    };

    value
        .split_whitespace()
        .map(trim_agent_id)
        .any(|agent| agent == primary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{SyncGranularity, is_intra_word_join};

    const SAMPLE: &str = include_str!("../../../tests/fixtures/sample_syllable.ttml");
    const SHOWCASE: &str = include_str!("../../../examples/lyrics/showcase.ttml");

    fn assert_track_invariants(track: &SyncedTrack) {
        for line in &track.lines {
            assert!(line.start_ms <= line.end_ms);
            for word in &line.words {
                let (start, end) = word.byte_range;
                assert!(start < end);
                assert!(end <= line.text.len());
                assert!(line.text.is_char_boundary(start));
                assert!(line.text.is_char_boundary(end));
                let slice = &line.text[start..end];
                assert_eq!(slice.trim(), slice);
                assert!(word.start_ms <= word.end_ms);
                assert!(word.start_ms >= line.start_ms);
                assert!(word.end_ms <= line.end_ms);
            }
            for pair in line.words.windows(2) {
                assert!(pair[0].end_ms <= pair[1].start_ms);
            }
        }

        for pair in track.lines.windows(2) {
            assert!(pair[0].start_ms < pair[1].start_ms);
            assert!(pair[0].end_ms < pair[1].start_ms);
        }
    }

    #[test]
    fn parses_sample_ttml_with_syllables_agents_background_and_annotations() {
        let track = parse_ttml(SAMPLE, "ttml-fixture").unwrap();

        assert_eq!(track.source_provider, "ttml-fixture");
        assert_eq!(track.granularity, SyncGranularity::Syllable);
        assert_track_invariants(&track);
        assert_eq!(track.lines.len(), 4);

        let first = &track.lines[0];
        assert_eq!(first.text, "Hello");
        assert_eq!(first.role, LineRole::Primary);
        assert!(is_intra_word_join(
            &first.text,
            &first.words[0],
            &first.words[1]
        ));
        assert!(first.annotations.iter().any(|annotation| {
            annotation.kind == AnnotationKind::Translation && annotation.text == "Halo"
        }));
        assert!(first.annotations.iter().any(|annotation| {
            annotation.kind == AnnotationKind::Romanization && annotation.text == "Herro"
        }));

        assert_eq!(track.lines[1].role, LineRole::DuetSecondary);
        assert_eq!(track.lines[1].text, "Second voice");
        assert!(track.lines[2].annotations.iter().any(|annotation| {
            annotation.kind == AnnotationKind::Background && annotation.text == "background"
        }));
        assert_eq!(track.lines[3].role, LineRole::BackgroundVocal);
        assert_eq!(track.lines[3].text, "Echo");
    }

    #[test]
    fn parses_showcase_ttml_with_every_annotation_kind_and_role() {
        let track = parse_ttml(SHOWCASE, "ttml-showcase").unwrap();

        assert_track_invariants(&track);
        assert_eq!(track.granularity, SyncGranularity::Syllable);

        let mut kinds = Vec::new();
        let mut roles = Vec::new();
        for line in &track.lines {
            roles.push(line.role);
            for annotation in &line.annotations {
                kinds.push(annotation.kind);
            }
        }
        for kind in [
            AnnotationKind::Translation,
            AnnotationKind::Romanization,
            AnnotationKind::Secondary,
            AnnotationKind::Background,
        ] {
            assert!(kinds.contains(&kind), "missing annotation kind {kind:?}");
        }
        for role in [
            LineRole::Primary,
            LineRole::DuetSecondary,
            LineRole::BackgroundVocal,
            LineRole::InstrumentalBreak,
        ] {
            assert!(roles.contains(&role), "missing line role {role:?}");
        }
    }

    #[test]
    fn parses_clock_and_second_timestamps() {
        assert_eq!(parse_timestamp("12.345s").unwrap(), 12_345);
        assert_eq!(parse_timestamp("12.345").unwrap(), 12_345);
        assert_eq!(parse_timestamp("00:01.250").unwrap(), 1_250);
        assert_eq!(parse_timestamp("01:02:03.400").unwrap(), 3_723_400);
        assert_eq!(parse_timestamp("1:02.5").unwrap(), 62_500);
    }

    #[test]
    fn rejects_malformed_timestamps() {
        assert!(parse_timestamp("").is_err());
        assert!(parse_timestamp("nope").is_err());
        assert!(parse_timestamp("00:99.000").is_err());
        assert!(parse_timestamp("00:01:99.000").is_err());
        assert!(parse_timestamp("NaNs").is_err());
        assert!(parse_timestamp("-1s").is_err());
    }

    #[test]
    fn parses_milliseconds_plus_and_comma_timestamps() {
        assert_eq!(parse_timestamp("500ms").unwrap(), 500);
        assert_eq!(parse_timestamp("+12.345s").unwrap(), 12_345);
        assert_eq!(parse_timestamp("00:01,250").unwrap(), 1_250);
        assert_eq!(parse_timestamp("+01:02.500").unwrap(), 62_500);
    }

    #[test]
    fn parses_line_with_br_element() {
        let xml = r#"<tt xmlns="http://www.w3.org/ns/ttml">
          <body><div>
            <p begin="1s" end="4s">
              <span begin="1s" end="2s">First</span><br/><span begin="2.5s" end="3.5s">Second</span>
            </p>
          </div></body>
        </tt>"#;
        let track = parse_ttml(xml, "test").unwrap();
        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].text, "First Second");
        assert_eq!(track.lines[0].words.len(), 2);
    }

    #[test]
    fn parses_line_without_begin_using_span_timing() {
        let xml = r#"<tt xmlns="http://www.w3.org/ns/ttml">
          <body><div>
            <p>
              <span begin="1s" end="2s">Hello</span> <span begin="2s" end="3s">World</span>
            </p>
          </div></body>
        </tt>"#;
        let track = parse_ttml(xml, "test").unwrap();
        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].start_ms, 1_000);
        assert_eq!(track.lines[0].text, "Hello World");
    }

    #[test]
    fn parses_duet_role_from_child_span_agent() {
        let xml = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata">
          <head><metadata><ttm:agent xml:id="v1"/><ttm:agent xml:id="v2"/></metadata></head>
          <body><div>
            <p begin="1s" end="2s"><span ttm:agent="v1">Singer 1</span></p>
            <p begin="3s" end="4s"><span ttm:agent="v2">Singer 2</span></p>
          </div></body>
        </tt>"#;
        let track = parse_ttml(xml, "test").unwrap();
        assert_eq!(track.lines[0].role, LineRole::Primary);
        assert_eq!(track.lines[1].role, LineRole::DuetSecondary);
    }

    #[test]
    fn rejects_xml_with_excessive_depth() {
        let mut xml = String::from("<tt><body><div>");
        for _ in 0..MAX_XML_DEPTH + 2 {
            xml.push_str("<g>");
        }
        xml.push_str(&"</g>".repeat(MAX_XML_DEPTH + 2));
        xml.push_str("</div></body></tt>");

        assert!(matches!(
            parse_ttml(&xml, "ttml"),
            Err(ProviderError::Invalid(_))
        ));
    }

    #[test]
    fn rejects_malformed_xml_without_panicking() {
        assert!(matches!(
            parse_ttml("<tt><body><div><p begin=\"1s\">oops", "ttml"),
            Err(ProviderError::Invalid(_))
        ));
    }
}
