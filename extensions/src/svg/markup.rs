//! Element nesting depth, measured from the raw text before any XML parser runs.
//!
//! The XML parser, usvg, and resvg all recurse per nesting level, so a few thousand levels
//! overflow the stack (aborting natively, trapping in wasm). This non-recursive scan refuses
//! documents deeper than [`super::MAX_NESTING`]. It is not a validating parser; it must only
//! never count less than the parser would.

use std::collections::HashMap;

use crate::api::ExtensionError;

/// Entity reference nesting roxmltree allows; deeper is reported with its error.
const MAX_ENTITY_DEPTH: usize = 10;

/// The deepest element nesting in `text`, with entity expansions included, or
/// [`ExtensionError::Budget`] once it passes `limit`.
pub(super) fn nesting(text: &str, limit: usize) -> Result<usize, ExtensionError> {
    let mut scan = Scan {
        entities: Vec::new(),
        depths: HashMap::new(),
        limit,
    };
    scan.content(text.as_bytes(), 0, 0)
}

struct Scan<'a> {
    /// Declared general entities in order; the first declaration of a name wins.
    entities: Vec<(&'a [u8], &'a [u8])>,
    /// Measured entity depths; `None` while measuring, to detect loops.
    depths: HashMap<&'a [u8], Option<usize>>,
    limit: usize,
}

impl<'a> Scan<'a> {
    /// The deepest nesting in `bytes`, starting `base` levels and `entities` references deep.
    fn content(
        &mut self,
        bytes: &'a [u8],
        base: usize,
        entities: usize,
    ) -> Result<usize, ExtensionError> {
        let mut depth = base;
        let mut deepest = base;
        let mut at = 0;
        while let Some(offset) = bytes[at..].iter().position(|&b| b == b'<' || b == b'&') {
            at += offset;
            let rest = &bytes[at..];
            if rest[0] == b'&' {
                let Some(end) = rest.iter().position(|&b| b == b';') else {
                    break;
                };
                let name = &rest[1..end];
                at += end + 1;
                if name.first() == Some(&b'#')
                    || matches!(name, b"lt" | b"gt" | b"amp" | b"apos" | b"quot")
                {
                    continue;
                }
                let nested = self.entity(name, entities)?;
                deepest = deepest.max(self.within(depth.saturating_add(nested))?);
                continue;
            }
            let skipped = if rest.starts_with(b"<!--") {
                after(rest, 4, b"-->")
            } else if rest.starts_with(b"<![CDATA[") {
                after(rest, 9, b"]]>")
            } else if rest.starts_with(b"<?") {
                after(rest, 2, b"?>")
            } else if rest.starts_with(b"<!DOCTYPE") {
                self.doctype(rest, 9)
            } else if rest.starts_with(b"</") {
                depth = depth.saturating_sub(1);
                after(rest, 2, b">")
            } else {
                let Some(end) = tag_end(rest) else {
                    break;
                };
                deepest = deepest.max(self.within(depth + 1)?);
                if rest[end - 1] != b'/' {
                    depth += 1;
                }
                Some(end + 1)
            };
            let Some(skipped) = skipped else {
                break;
            };
            at += skipped;
        }
        Ok(deepest)
    }

    fn within(&self, depth: usize) -> Result<usize, ExtensionError> {
        if depth > self.limit {
            return Err(ExtensionError::Budget(format!(
                "the SVG's elements nest more than {} deep",
                self.limit
            )));
        }
        Ok(depth)
    }

    /// Levels a reference to entity `name` adds: one plus its replacement text's depth.
    fn entity(&mut self, name: &'a [u8], entities: usize) -> Result<usize, ExtensionError> {
        let loop_error = || {
            ExtensionError::Malformed(
                "SVG data parsing failed cause a possible entity reference loop is detected".into(),
            )
        };
        match self.depths.get(name) {
            Some(Some(depth)) => return Ok(*depth),
            Some(None) => return Err(loop_error()),
            None => {}
        }
        if entities >= MAX_ENTITY_DEPTH {
            return Err(loop_error());
        }
        // An unknown entity is the parser's error.
        let Some(&(_, value)) = self.entities.iter().find(|(declared, _)| *declared == name) else {
            return Ok(0);
        };
        self.depths.insert(name, None);
        let depth = self.content(value, 0, entities + 1)? + 1;
        self.depths.insert(name, Some(depth));
        Ok(depth)
    }

    /// The end of a doctype declaration in `rest`, recording its general entities.
    fn doctype(&mut self, rest: &'a [u8], mut at: usize) -> Option<usize> {
        // The name and external identifier, up to the internal subset or the end.
        loop {
            match *rest.get(at)? {
                quote @ (b'"' | b'\'') => at += 1 + find(&rest[at + 1..], &[quote])? + 1,
                b'[' => break,
                b'>' => return Some(at + 1),
                _ => at += 1,
            }
        }
        at += 1;
        loop {
            let subset = &rest[at..];
            let next = subset.iter().position(|b| !b.is_ascii_whitespace())?;
            at += next;
            let subset = &rest[at..];
            at += if subset.starts_with(b"]") {
                return Some(at + after(subset, 1, b">")?);
            } else if subset.starts_with(b"<!--") {
                after(subset, 4, b"-->")?
            } else if subset.starts_with(b"<?") {
                after(subset, 2, b"?>")?
            } else if subset.starts_with(b"<!ENTITY") {
                self.entity_declaration(subset)?
            } else if subset.starts_with(b"<") {
                tag_end(subset)? + 1
            } else {
                // A parameter entity reference, or what the parser refuses.
                1
            };
        }
    }

    /// The end of an entity declaration, recording a general entity's replacement text.
    fn entity_declaration(&mut self, decl: &'a [u8]) -> Option<usize> {
        let end = tag_end(decl)?;
        let mut words = decl[8..end].trim_ascii_start();
        let general = !words.starts_with(b"%");
        if !general {
            words = words[1..].trim_ascii_start();
        }
        let name_end = words
            .iter()
            .position(|b| b.is_ascii_whitespace() || matches!(b, b'"' | b'\''))?;
        let name = &words[..name_end];
        let definition = words[name_end..].trim_ascii_start();
        if general
            && let Some(&quote) = definition.first()
            && matches!(quote, b'"' | b'\'')
            && let Some(close) = find(&definition[1..], &[quote])
            && !self.entities.iter().any(|(declared, _)| *declared == name)
        {
            self.entities.push((name, &definition[1..=close]));
        }
        Some(end + 1)
    }
}

fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The offset just past the first `closing` after `from`.
fn after(bytes: &[u8], from: usize, closing: &[u8]) -> Option<usize> {
    Some(from + find(bytes.get(from..)?, closing)? + closing.len())
}

/// The `>` ending the tag at the start of `bytes`, skipping quoted values.
fn tag_end(bytes: &[u8]) -> Option<usize> {
    let mut at = 1;
    loop {
        match *bytes.get(at)? {
            quote @ (b'"' | b'\'') => at += 1 + find(&bytes[at + 1..], &[quote])? + 1,
            b'>' => return Some(at),
            _ => at += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::nesting;

    fn depth(text: &str) -> usize {
        nesting(text, usize::MAX).expect("measured")
    }

    #[test]
    fn elements_nest_as_they_open_and_close() {
        assert_eq!(depth("<svg/>"), 1);
        assert_eq!(depth("<svg><g><rect/></g><g/></svg>"), 3);
        assert_eq!(
            depth(r#"<svg a="<g>" b='>'><!-- <g><g> --><![CDATA[<g><g>]]><?pi <g>?><g></g></svg>"#),
            2
        );
        let deep = format!("<svg>{}{}</svg>", "<g>".repeat(40), "</g>".repeat(40));
        assert_eq!(depth(&deep), 41);
        assert!(nesting(&deep, 40).is_err());
        assert_eq!(nesting(&deep, 41).ok(), Some(41));
    }

    #[test]
    fn an_entity_nests_as_deep_as_its_replacement_text_where_it_is_used() {
        let text = r#"<?xml version="1.0"?>
<!DOCTYPE svg [
  <!ENTITY ns "http://www.w3.org/2000/svg">
  <!ENTITY one "<g><g/></g>">
  <!ENTITY two '<g>&one;&one;</g>'>
  <!ENTITY % parameter "<g><g><g><g></g></g></g></g>">
  <!-- <!ENTITY three "<g><g><g></g></g></g>"> -->
]>
<svg xmlns="&ns;"><g>&two;</g></svg>"#;
        // svg, g, then two: its entity level, g, one's entity level, g, g.
        assert_eq!(depth(text), 7);
    }

    #[test]
    fn entities_inside_one_another_too_deep_or_in_a_loop_are_refused() {
        let declarations: String = std::iter::once(r#"<!ENTITY e0 "<g/>">"#.to_owned())
            .chain((1..=11).map(|level| format!(r#"<!ENTITY e{level} "&e{};">"#, level - 1)))
            .collect();
        let text = |entity: usize| format!("<!DOCTYPE svg [{declarations}]><svg>&e{entity};</svg>");
        assert_eq!(depth(&text(9)), 12);
        assert!(nesting(&text(10), usize::MAX).is_err());
        let looping = r#"<!DOCTYPE svg [<!ENTITY a "&b;"><!ENTITY b "&a;">]><svg>&a;</svg>"#;
        assert!(nesting(looping, usize::MAX).is_err());
    }
}
