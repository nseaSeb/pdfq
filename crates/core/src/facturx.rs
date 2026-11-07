//! Factur-X XMP metadata.
//!
//! Typst (krilla) cannot write custom XMP, so the metadata stream of the exported
//! PDF is rewritten in place: the object is replaced and the cross-reference
//! table shifted. This relies on krilla's output layout (classic xref table with
//! one subsection, unfiltered metadata stream) and fails loudly if it changes:
//! a PDF is never returned without its Factur-X metadata.

use pdfq_protocol::{FACTURX_FILENAME, FacturxProfile};

const FX_NS: &str = "urn:factur-x:pdfa:CrossIndustryDocument:invoice:1p0#";

/// Adds the Factur-X properties and their PDF/A extension schema to the XMP.
pub fn add_metadata(pdf: &[u8], profile: FacturxProfile) -> Result<Vec<u8>, String> {
    let xref = Xref::parse(pdf)?;
    let root = xref.trailer_ref(pdf, b"/Root")?;
    let catalog = object(pdf, xref.offset(root)?)?;
    let meta =
        dict_ref(&pdf[catalog.clone()], b"/Metadata").ok_or("the catalog has no /Metadata")?;
    let meta_offset = xref.offset(meta)?;
    let (span, data) = stream_object(pdf, meta_offset)?;
    let xmp = &pdf[data];
    let xmp = std::str::from_utf8(xmp).map_err(|_| "the XMP is not UTF-8")?;
    let xmp = rewrite_xmp(xmp, profile)?;

    let mut object = format!(
        "{meta} 0 obj\n<</Type/Metadata/Subtype/XML/Length {}>>\nstream\n",
        xmp.len()
    )
    .into_bytes();
    object.extend_from_slice(xmp.as_bytes());
    object.extend_from_slice(b"\nendstream\nendobj");
    let delta = object.len() as i64 - span.len() as i64;
    let shift = |offset: u64| {
        if offset > meta_offset as u64 {
            (offset as i64 + delta) as u64
        } else {
            offset
        }
    };

    let mut out = Vec::with_capacity(pdf.len() + object.len());
    out.extend_from_slice(&pdf[..span.start]);
    out.extend_from_slice(&object);
    out.extend_from_slice(&pdf[span.end..xref.start]);
    out.extend_from_slice(format!("xref\n0 {}\n", xref.entries.len()).as_bytes());
    for entry in &xref.entries {
        let line = match entry {
            Entry::InUse(offset, generation) => {
                format!("{:010} {generation:05} n\r\n", shift(*offset))
            }
            Entry::Free(raw) => raw.clone(),
        };
        out.extend_from_slice(line.as_bytes());
    }
    let trailer = &pdf[xref.entries_end..xref.startxref_keyword];
    out.extend_from_slice(trailer);
    out.extend_from_slice(format!("startxref\n{}\n%%EOF\n", shift(xref.start as u64)).as_bytes());
    Ok(out)
}

fn rewrite_xmp(xmp: &str, profile: FacturxProfile) -> Result<String, String> {
    if !xmp.contains("<pdfaid:part>3</pdfaid:part>") {
        return Err("Factur-X requires PDF/A-3".into());
    }
    let properties = format!(
        "<fx:DocumentType>INVOICE</fx:DocumentType>\
         <fx:DocumentFileName>{FACTURX_FILENAME}</fx:DocumentFileName>\
         <fx:Version>1.0</fx:Version>\
         <fx:ConformanceLevel>{}</fx:ConformanceLevel>",
        profile.conformance_level()
    );
    let property = |name: &str, description: &str| {
        format!(
            "<rdf:li rdf:parseType=\"Resource\">\
             <pdfaProperty:name>{name}</pdfaProperty:name>\
             <pdfaProperty:valueType>Text</pdfaProperty:valueType>\
             <pdfaProperty:category>external</pdfaProperty:category>\
             <pdfaProperty:description>{description}</pdfaProperty:description></rdf:li>"
        )
    };
    let schema = format!(
        "<rdf:li rdf:parseType=\"Resource\">\
         <pdfaSchema:schema>Factur-X PDFA Extension Schema</pdfaSchema:schema>\
         <pdfaSchema:namespaceURI>{FX_NS}</pdfaSchema:namespaceURI>\
         <pdfaSchema:prefix>fx</pdfaSchema:prefix>\
         <pdfaSchema:property><rdf:Seq>{}{}{}{}</rdf:Seq></pdfaSchema:property></rdf:li>",
        property("DocumentFileName", "The name of the embedded XML document"),
        property(
            "DocumentType",
            "The type of the hybrid document in capital letters, e.g. INVOICE or ORDER"
        ),
        property(
            "Version",
            "The actual version of the standard applying to the embedded XML document"
        ),
        property(
            "ConformanceLevel",
            "The conformance level of the embedded XML document"
        ),
    );
    let xmp = insert_before(xmp, "</rdf:Bag></pdfaExtension:schemas>", &schema)?;
    let xmp = insert_before(&xmp, "</rdf:Description></rdf:RDF>", &properties)?;
    let opening = "<rdf:Description rdf:about=\"\"";
    let namespace = format!("{opening} xmlns:fx=\"{FX_NS}\"");
    let xmp = replace_once(&xmp, opening, &namespace)?;
    Ok(xmp)
}

fn insert_before(haystack: &str, anchor: &str, text: &str) -> Result<String, String> {
    replace_once(haystack, anchor, &format!("{text}{anchor}"))
}

/// Replaces an anchor that must occur exactly once.
fn replace_once(haystack: &str, anchor: &str, replacement: &str) -> Result<String, String> {
    if haystack.matches(anchor).count() != 1 {
        return Err(format!("unexpected XMP layout around `{anchor}`"));
    }
    Ok(haystack.replacen(anchor, replacement, 1))
}

enum Entry {
    InUse(u64, u32),
    /// Kept verbatim: free entries hold object numbers, not offsets.
    Free(String),
}

struct Xref {
    /// Offset of the `xref` keyword.
    start: usize,
    entries: Vec<Entry>,
    /// End of the last entry (start of the trailer).
    entries_end: usize,
    /// Offset of the final `startxref` keyword.
    startxref_keyword: usize,
}

impl Xref {
    /// Parses a classic cross-reference table with a single subsection.
    fn parse(pdf: &[u8]) -> Result<Self, String> {
        let keyword = rfind(pdf, b"startxref").ok_or("no startxref")?;
        let start: usize = number_at(pdf, keyword + b"startxref".len())
            .ok_or("invalid startxref")?
            .0 as usize;
        let mut pos = start;
        if !pdf.get(start..).is_some_and(|s| s.starts_with(b"xref")) {
            return Err("not a classic xref table".into());
        }
        pos += 4;
        let (first, next) = number_at(pdf, pos).ok_or("invalid xref subsection")?;
        let (count, next) = number_at(pdf, next).ok_or("invalid xref subsection")?;
        if first != 0 {
            return Err("unexpected xref subsection".into());
        }
        pos = skip_whitespace(pdf, next);
        let mut entries = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let raw = pdf.get(pos..pos + 20).ok_or("truncated xref")?;
            let raw = std::str::from_utf8(raw).map_err(|_| "invalid xref entry")?;
            let offset = raw[..10].parse().map_err(|_| "invalid xref entry")?;
            let generation = raw[11..16].parse().map_err(|_| "invalid xref entry")?;
            entries.push(match &raw[17..18] {
                "n" => Entry::InUse(offset, generation),
                "f" => Entry::Free(raw.to_owned()),
                _ => return Err("invalid xref entry".into()),
            });
            pos += 20;
        }
        if !pdf[pos..].starts_with(b"trailer") {
            return Err("more than one xref subsection".into());
        }
        Ok(Self {
            start,
            entries,
            entries_end: pos,
            startxref_keyword: keyword,
        })
    }

    fn offset(&self, object: u32) -> Result<usize, String> {
        match self.entries.get(object as usize) {
            Some(Entry::InUse(offset, 0)) => Ok(*offset as usize),
            _ => Err(format!("object {object} not in the xref table")),
        }
    }

    fn trailer_ref(&self, pdf: &[u8], key: &[u8]) -> Result<u32, String> {
        dict_ref(&pdf[self.entries_end..self.startxref_keyword], key)
            .ok_or_else(|| format!("no {} in the trailer", String::from_utf8_lossy(key)))
    }
}

/// The span of the object at `offset`, from `N 0 obj` to the end of `endobj`.
fn object(pdf: &[u8], offset: usize) -> Result<std::ops::Range<usize>, String> {
    let rest = pdf.get(offset..).ok_or("object offset out of range")?;
    let end = find(rest, b"endobj").ok_or("unterminated object")?;
    Ok(offset..offset + end + b"endobj".len())
}

/// The span of the stream object at `offset` and of its data, using `/Length`
/// (the data may contain any text, `endobj` included).
fn stream_object(
    pdf: &[u8],
    offset: usize,
) -> Result<(std::ops::Range<usize>, std::ops::Range<usize>), String> {
    let object = pdf.get(offset..).ok_or("object offset out of range")?;
    let keyword = find(object, b"stream").ok_or("not a stream")?;
    let dict = &object[..keyword];
    if find(dict, b"/Filter").is_some() {
        return Err("the metadata stream is filtered".into());
    }
    let length = find(dict, b"/Length")
        .and_then(|i| number_at(dict, i + b"/Length".len()))
        .ok_or("no stream /Length")?
        .0 as usize;
    let mut start = keyword + b"stream".len();
    if object[start..].starts_with(b"\r\n") {
        start += 2;
    } else if object[start..].starts_with(b"\n") {
        start += 1;
    }
    let end = start + length;
    let rest = object.get(end..).ok_or("truncated stream")?;
    let after = skip_whitespace(rest, 0);
    if !rest[after..].starts_with(b"endstream") {
        return Err("stream /Length mismatch".into());
    }
    let endobj = after + b"endstream".len();
    let endobj = skip_whitespace(rest, endobj);
    if !rest[endobj..].starts_with(b"endobj") {
        return Err("unterminated object".into());
    }
    let object_end = offset + end + endobj + b"endobj".len();
    Ok((offset..object_end, offset + start..offset + end))
}

/// The object number of `key N 0 R` in a dictionary.
fn dict_ref(dict: &[u8], key: &[u8]) -> Option<u32> {
    let mut from = 0;
    // Skip keys that are only a prefix (`/Root` vs `/RootX`).
    while let Some(i) = find(&dict[from..], key) {
        let after = from + i + key.len();
        if let Some((number, next)) = number_at(dict, after)
            && let Some((0, next)) = number_at(dict, next)
            && dict[skip_whitespace(dict, next)..].starts_with(b"R")
        {
            return u32::try_from(number).ok();
        }
        from = after;
    }
    None
}

/// Parses an unsigned integer after optional whitespace.
fn number_at(data: &[u8], pos: usize) -> Option<(u64, usize)> {
    let start = skip_whitespace(data, pos);
    let len = data[start..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    let text = std::str::from_utf8(&data[start..start + len]).ok()?;
    Some((text.parse().ok()?, start + len))
}

fn skip_whitespace(data: &[u8], pos: usize) -> usize {
    pos + data[pos.min(data.len())..]
        .iter()
        .take_while(|b| b.is_ascii_whitespace())
        .count()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}
