use std::borrow::Cow;

use imap_proto::{AttributeValue, BodyStructure, Envelope, MessageSection, Response, SectionPath};
use jiff::Zoned;

use super::{Flag, Seq, Uid};
use crate::types::ResponseData;

/// Format of Date and Time as defined RFC3501.
/// See `date-time` element in [Formal Syntax](https://tools.ietf.org/html/rfc3501#section-9)
/// chapter of this RFC.
const DATE_TIME_FORMAT: &str = "%d-%b-%Y %H:%M:%S %z";

fn parse_internal_date(input: &str) -> Option<Zoned> {
    // Chrono's %z requires a sign and two digits each for hours and minutes,
    // with any number of colons or whitespace between the two fields. Jiff's
    // %z instead accepts offset seconds. Normalize only the old minute-precision
    // grammar before handing the complete date back to Jiff.
    let offset_start = input.rfind(['+', '-', '−'])?;
    let (date_time, offset) = input.split_at(offset_start);
    let sign = offset.chars().next()?;
    let offset = &offset[sign.len_utf8()..];
    let hours = offset.get(..2)?;
    let minutes = offset
        .get(2..)?
        .trim_start_matches(|c: char| c == ':' || c.is_whitespace());
    if !hours.bytes().all(|byte| byte.is_ascii_digit())
        || minutes.len() != 2
        || !minutes.bytes().all(|byte| byte.is_ascii_digit())
        || hours.parse::<u8>().ok()? > 23
        || minutes.parse::<u8>().ok()? > 59
    {
        return None;
    }

    // Jiff normalizes leap seconds to 59. Do not silently change an IMAP
    // timestamp that the replacement representation cannot preserve. Numeric
    // directives accept leading whitespace, so inspect the complete seconds
    // field after separating the offset, not a whitespace-delimited word.
    if date_time.rsplit(':').next()?.trim() == "60" {
        return None;
    }
    let sign = if sign == '+' { '+' } else { '-' };
    let normalized = format!("{date_time}{sign}{hours}{minutes}");
    Zoned::strptime(DATE_TIME_FORMAT, normalized).ok()
}

/// An IMAP [`FETCH` response](https://tools.ietf.org/html/rfc3501#section-7.4.2) that contains
/// data about a particular message.
///
/// This response occurs as the result of a `FETCH` or `STORE`
/// command, as well as by unilateral server decision (e.g., flag updates).
#[derive(Debug)]
pub struct Fetch {
    response: ResponseData,
    /// The ordinal number of this message in its containing mailbox.
    pub message: Seq,

    /// A number expressing the unique identifier of the message.
    /// Only present if `UID` was specified in the query argument to `FETCH` and the server
    /// supports UIDs.
    pub uid: Option<Uid>,

    /// A number expressing the [RFC-2822](https://tools.ietf.org/html/rfc2822) size of the message.
    /// Only present if `RFC822.SIZE` was specified in the query argument to `FETCH`.
    pub size: Option<u32>,

    /// A number expressing the [RFC-7162](https://tools.ietf.org/html/rfc7162) mod-sequence
    /// of the message.
    pub modseq: Option<u64>,
}

impl Fetch {
    pub(crate) fn new(response: ResponseData) -> Self {
        let (message, uid, size, modseq) =
            if let Response::Fetch(message, attrs) = response.parsed() {
                let mut uid = None;
                let mut size = None;
                let mut modseq = None;

                for attr in attrs {
                    match attr {
                        AttributeValue::Uid(id) => uid = Some(*id),
                        AttributeValue::Rfc822Size(sz) => size = Some(*sz),
                        AttributeValue::ModSeq(ms) => modseq = Some(*ms),
                        _ => {}
                    }
                }
                (*message, uid, size, modseq)
            } else {
                unreachable!()
            };

        Fetch {
            response,
            message,
            uid,
            size,
            modseq,
        }
    }

    /// A list of flags that are set for this message.
    pub fn flags(&self) -> impl Iterator<Item = Flag<'_>> {
        if let Response::Fetch(_, attrs) = self.response.parsed() {
            attrs
                .iter()
                .filter_map(|attr| match attr {
                    AttributeValue::Flags(raw_flags) => {
                        Some(raw_flags.iter().map(|s| Flag::from(s.as_ref())))
                    }
                    _ => None,
                })
                .flatten()
        } else {
            unreachable!()
        }
    }

    /// The bytes that make up the header of this message, if `BODY[HEADER]`, `BODY.PEEK[HEADER]`,
    /// or `RFC822.HEADER` was included in the `query` argument to `FETCH`.
    pub fn header(&self) -> Option<&[u8]> {
        if let Response::Fetch(_, attrs) = self.response.parsed() {
            attrs
                .iter()
                .filter_map(|av| match av {
                    AttributeValue::BodySection {
                        section: Some(SectionPath::Full(MessageSection::Header)),
                        data: Some(hdr),
                        ..
                    }
                    | AttributeValue::Rfc822Header(Some(hdr)) => Some(hdr.as_ref()),
                    _ => None,
                })
                .next()
        } else {
            unreachable!()
        }
    }

    /// The bytes that make up this message, included if `BODY[]` or `RFC822` was included in the
    /// `query` argument to `FETCH`. The bytes SHOULD be interpreted by the client according to the
    /// content transfer encoding, body type, and subtype.
    pub fn body(&self) -> Option<&[u8]> {
        if let Response::Fetch(_, attrs) = self.response.parsed() {
            attrs
                .iter()
                .filter_map(|av| match av {
                    AttributeValue::BodySection {
                        section: None,
                        data: Some(body),
                        ..
                    }
                    | AttributeValue::Rfc822(Some(body)) => Some(body.as_ref()),
                    _ => None,
                })
                .next()
        } else {
            unreachable!()
        }
    }

    /// The bytes that make up the text of this message, included if `BODY[TEXT]`, `RFC822.TEXT`,
    /// or `BODY.PEEK[TEXT]` was included in the `query` argument to `FETCH`. The bytes SHOULD be
    /// interpreted by the client according to the content transfer encoding, body type, and
    /// subtype.
    pub fn text(&self) -> Option<&[u8]> {
        if let Response::Fetch(_, attrs) = self.response.parsed() {
            attrs
                .iter()
                .filter_map(|av| match av {
                    AttributeValue::BodySection {
                        section: Some(SectionPath::Full(MessageSection::Text)),
                        data: Some(body),
                        ..
                    }
                    | AttributeValue::Rfc822Text(Some(body)) => Some(body.as_ref()),
                    _ => None,
                })
                .next()
        } else {
            unreachable!()
        }
    }

    /// The envelope of this message, if `ENVELOPE` was included in the `query` argument to
    /// `FETCH`. This is computed by the server by parsing the
    /// [RFC-2822](https://tools.ietf.org/html/rfc2822) header into the component parts, defaulting
    /// various fields as necessary.
    ///
    /// The full description of the format of the envelope is given in [RFC 3501 section
    /// 7.4.2](https://tools.ietf.org/html/rfc3501#section-7.4.2).
    pub fn envelope(&self) -> Option<&Envelope<'_>> {
        if let Response::Fetch(_, attrs) = self.response.parsed() {
            attrs
                .iter()
                .filter_map(|av| match av {
                    AttributeValue::Envelope(env) => Some(&**env),
                    _ => None,
                })
                .next()
        } else {
            unreachable!()
        }
    }

    /// Extract the bytes that makes up the given `BOD[<section>]` of a `FETCH` response.
    ///
    /// See [section 7.4.2 of RFC 3501](https://tools.ietf.org/html/rfc3501#section-7.4.2) for
    /// details.
    pub fn section(&self, path: &SectionPath) -> Option<&[u8]> {
        if let Response::Fetch(_, attrs) = self.response.parsed() {
            attrs
                .iter()
                .filter_map(|av| match av {
                    AttributeValue::BodySection {
                        section: Some(sp),
                        data: Some(data),
                        ..
                    } if sp == path => Some(data.as_ref()),
                    _ => None,
                })
                .next()
        } else {
            unreachable!()
        }
    }

    /// Extract the `INTERNALDATE` of a `FETCH` response
    ///
    /// See [section 2.3.3 of RFC 3501](https://tools.ietf.org/html/rfc3501#section-2.3.3) for
    /// details.
    ///
    /// The returned value preserves the server's fixed UTC offset. An absent
    /// or invalid date, including an unrepresentable leap second, yields `None`.
    pub fn internal_date(&self) -> Option<Zoned> {
        if let Response::Fetch(_, attrs) = self.response.parsed() {
            attrs
                .iter()
                .filter_map(|av| match av {
                    AttributeValue::InternalDate(date_time) => Some(date_time.as_ref()),
                    _ => None,
                })
                .next()
                .and_then(parse_internal_date)
        } else {
            unreachable!()
        }
    }

    /// Extract the `BODYSTRUCTURE` of a `FETCH` response
    ///
    /// See [section 2.3.6 of RFC 3501](https://tools.ietf.org/html/rfc3501#section-2.3.6) for
    /// details.
    pub fn bodystructure(&self) -> Option<&BodyStructure<'_>> {
        if let Response::Fetch(_, attrs) = self.response.parsed() {
            attrs
                .iter()
                .filter_map(|av| match av {
                    AttributeValue::BodyStructure(bs) => Some(bs),
                    _ => None,
                })
                .next()
        } else {
            unreachable!()
        }
    }

    /// Extract the `X-GM-LABELS` of a `FETCH` response
    ///
    /// See [Access to Gmail labels: X-GM-LABELS](https://developers.google.com/gmail/imap/imap-extensions#access_to_labels_x-gm-labels)
    /// for details.
    pub fn gmail_labels(&self) -> Option<&Vec<Cow<'_, str>>> {
        if let Response::Fetch(_, attrs) = self.response.parsed() {
            attrs
                .iter()
                .filter_map(|av| match av {
                    AttributeValue::GmailLabels(gl) => Some(gl),
                    _ => None,
                })
                .next()
        } else {
            unreachable!()
        }
    }

    /// Extract the `X-GM-MSGID` of a `FETCH` response
    ///
    /// See [Access to the Gmail unique message ID: X-GM-MSGID](https://developers.google.com/workspace/gmail/imap/imap-extensions#access_to_the_unique_message_id_x-gm-msgid)
    /// for details.
    pub fn gmail_msg_id(&self) -> Option<&u64> {
        if let Response::Fetch(_, attrs) = self.response.parsed() {
            attrs
                .iter()
                .filter_map(|av| match av {
                    AttributeValue::GmailMsgId(id) => Some(id),
                    _ => None,
                })
                .next()
        } else {
            unreachable!()
        }
    }
}
