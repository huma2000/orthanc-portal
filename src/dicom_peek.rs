//! # dicom_peek.rs
//!
//! Reads the **StudyInstanceUID** from the start of a DICOM file, before the file is passed on to
//! Orthanc. Used by upload links (`routes/inbound.rs`): a file sent from outside may only start a
//! new study or add to a study that came in through the same link, never add images to a study
//! already in the archive. Orthanc files an image under an existing study when its identifiers
//! match, so the check has to happen before Orthanc sees the file.
//!
//! Only what is needed is understood: a DICOM Part 10 file (128-byte preamble, `DICM`, the file
//! meta group in explicit VR little endian) whose data set is explicit or implicit VR little
//! endian, which covers uncompressed and compressed images alike (compression only concerns the
//! pixel data). Sequences of defined or undefined length before the UID are skipped. Anything
//! else (no preamble, big endian, a deflated data set, a ZIP archive) is "not readable", and upload
//! links refuse it.

/// The tag sought: (0020,000D) StudyInstanceUID.
const STUDY_UID: (u16, u16) = (0x0020, 0x000D);
/// Transfer syntaxes whose data set this reader cannot walk.
const IMPLICIT_LE: &str = "1.2.840.10008.1.2";
const EXPLICIT_BE: &str = "1.2.840.10008.1.2.2";
const DEFLATED: &str = "1.2.840.10008.1.2.1.99";
/// Nesting depth of sequences followed at most.
const MAX_DEPTH: usize = 8;

#[derive(Debug, PartialEq, Eq)]
pub enum Peek {
    /// The StudyInstanceUID (digits and dots).
    Found(String),
    /// The beginning read so far is fine, but the UID is further on: give more bytes.
    NeedMore,
    /// Not a DICOM file this reader understands, or one without a valid StudyInstanceUID.
    Unreadable,
}

/// A failure while walking: out of bytes (more may come) or a broken file.
enum Stop {
    More,
    Bad,
}

/// One element header: its tag, its VR ("  " when implicit) and its length.
type Header = ((u16, u16), [u8; 2], u32);

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    explicit: bool,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Stop> {
        let end = self.pos.checked_add(n).ok_or(Stop::Bad)?;
        if end > self.data.len() {
            return Err(Stop::More);
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }
    fn u16(&mut self) -> Result<u16, Stop> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, Stop> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn skip(&mut self, n: u32) -> Result<(), Stop> {
        self.take(n as usize).map(|_| ())
    }

    /// One element header: (tag, VR (or "  " when implicit), length).
    fn header(&mut self, explicit: bool) -> Result<Header, Stop> {
        let tag = (self.u16()?, self.u16()?);
        // Item and delimiter tags never carry a VR.
        if tag.0 == 0xFFFE || !explicit {
            return Ok((tag, *b"  ", self.u32()?));
        }
        let vr = self.take(2)?;
        let vr = [vr[0], vr[1]];
        if !vr.iter().all(u8::is_ascii_uppercase) {
            return Err(Stop::Bad);
        }
        let long = matches!(
            &vr,
            b"OB" | b"OW" | b"OF" | b"SQ" | b"UT" | b"UN" | b"UC" | b"UR" | b"OD" | b"OL" | b"OV" | b"SV" | b"UV"
        );
        let length = if long {
            self.take(2)?;
            self.u32()?
        } else {
            u32::from(self.u16()?)
        };
        Ok((tag, vr, length))
    }

    /// Skips a sequence of undefined length (its items), up to its delimiter.
    fn skip_sequence(&mut self, depth: usize) -> Result<(), Stop> {
        if depth > MAX_DEPTH {
            return Err(Stop::Bad);
        }
        loop {
            let (tag, _, length) = self.header(false)?;
            match tag {
                (0xFFFE, 0xE0DD) => return Ok(()), // end of the sequence
                (0xFFFE, 0xE000) if length == u32::MAX => self.skip_item(depth + 1)?,
                (0xFFFE, 0xE000) => self.skip(length)?,
                _ => return Err(Stop::Bad),
            }
        }
    }

    /// Skips the elements of an item of undefined length, up to its delimiter.
    fn skip_item(&mut self, depth: usize) -> Result<(), Stop> {
        loop {
            let (tag, vr, length) = self.header(self.explicit)?;
            if tag == (0xFFFE, 0xE00D) {
                return Ok(());
            }
            self.skip_value(vr, length, depth)?;
        }
    }

    fn skip_value(&mut self, vr: [u8; 2], length: u32, depth: usize) -> Result<(), Stop> {
        if length == u32::MAX {
            // Undefined length: a sequence (or, in implicit VR, an element known to be one).
            if &vr == b"SQ" || &vr == b"UN" || &vr == b"  " {
                return self.skip_sequence(depth);
            }
            return Err(Stop::Bad); // encapsulated pixel data comes long after the UID
        }
        self.skip(length)
    }
}

/// Looks for the StudyInstanceUID in the first bytes of a file.
pub fn study_uid(data: &[u8]) -> Peek {
    if data.len() < 132 {
        return Peek::NeedMore;
    }
    if &data[128..132] != b"DICM" {
        return Peek::Unreadable;
    }
    let mut r = Reader { data, pos: 132, explicit: true };
    match walk(&mut r) {
        Ok(Some(uid)) => Peek::Found(uid),
        Ok(None) | Err(Stop::Bad) => Peek::Unreadable,
        Err(Stop::More) => Peek::NeedMore,
    }
}

fn walk(r: &mut Reader) -> Result<Option<String>, Stop> {
    // The file meta group: always explicit VR little endian; the transfer syntax is in it.
    let mut syntax = String::new();
    loop {
        let start = r.pos;
        let group = {
            let b = r.data.get(r.pos..r.pos + 2).ok_or(Stop::More)?;
            u16::from_le_bytes([b[0], b[1]])
        };
        if group != 0x0002 {
            r.pos = start;
            break;
        }
        let (tag, vr, length) = r.header(true)?;
        if length == u32::MAX || length > 1024 {
            return Err(Stop::Bad);
        }
        let value = r.take(length as usize)?;
        if tag == (0x0002, 0x0010) {
            syntax = String::from_utf8_lossy(value).trim_end_matches(['\0', ' ']).to_string();
        }
        let _ = vr;
    }
    match syntax.as_str() {
        "" | EXPLICIT_BE | DEFLATED => return Err(Stop::Bad),
        IMPLICIT_LE => r.explicit = false,
        _ => r.explicit = true,
    }
    // The data set: elements in increasing tag order, so the walk stops once past the UID.
    loop {
        let explicit = r.explicit;
        let (tag, vr, length) = r.header(explicit)?;
        if tag == STUDY_UID {
            if length > 64 || length == u32::MAX {
                return Err(Stop::Bad);
            }
            let value = r.take(length as usize)?;
            let uid = String::from_utf8_lossy(value).trim_end_matches(['\0', ' ']).to_string();
            return Ok(crate::util::is_valid_dicom_uid(&uid).then_some(uid));
        }
        if tag > STUDY_UID {
            return Ok(None); // no StudyInstanceUID
        }
        r.skip_value(vr, length, 0)?;
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// One explicit VR little endian element.
    fn element(group: u16, elem: u16, vr: &[u8; 2], value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(group.to_le_bytes());
        out.extend(elem.to_le_bytes());
        out.extend(vr);
        let mut value = value.to_vec();
        if value.len() % 2 == 1 {
            value.push(if vr == b"UI" { 0 } else { b' ' });
        }
        if matches!(vr, b"OB" | b"SQ" | b"UN" | b"UT") {
            out.extend([0, 0]);
            out.extend((value.len() as u32).to_le_bytes());
        } else {
            out.extend((value.len() as u16).to_le_bytes());
        }
        out.extend(value);
        out
    }

    /// A small DICOM file: preamble, meta group with `syntax`, then `body`.
    pub fn file(syntax: &str, body: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; 128];
        out.extend(b"DICM");
        out.extend(element(0x0002, 0x0010, b"UI", syntax.as_bytes()));
        out.extend(body);
        out
    }

    const UID: &str = "1.2.826.0.1.3680043.8.498.77";

    #[test]
    fn the_uid_of_an_explicit_little_endian_file() {
        let mut body = element(0x0008, 0x0060, b"CS", b"CT");
        body.extend(element(0x0010, 0x0020, b"LO", b"P-001"));
        body.extend(element(0x0020, 0x000D, b"UI", UID.as_bytes()));
        body.extend(element(0x7FE0, 0x0010, b"OB", &[0u8; 64]));
        let f = file("1.2.840.10008.1.2.4.50", &body);
        assert_eq!(study_uid(&f), Peek::Found(UID.into()));
        // Cut before the UID: more is needed; cut inside the preamble too.
        assert_eq!(study_uid(&f[..150]), Peek::NeedMore);
        assert_eq!(study_uid(&f[..20]), Peek::NeedMore);
    }

    #[test]
    fn sequences_before_the_uid_are_skipped() {
        // (0008,1110) ReferencedStudySequence, undefined length, one item of undefined length.
        let mut seq = Vec::new();
        seq.extend(0x0008u16.to_le_bytes());
        seq.extend(0x1110u16.to_le_bytes());
        seq.extend(b"SQ\0\0");
        seq.extend(u32::MAX.to_le_bytes());
        seq.extend([0xFE, 0xFF, 0x00, 0xE0]);
        seq.extend(u32::MAX.to_le_bytes());
        seq.extend(element(0x0008, 0x1155, b"UI", b"9.9.9"));
        seq.extend([0xFE, 0xFF, 0x0D, 0xE0, 0, 0, 0, 0]);
        seq.extend([0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
        let mut body = seq;
        body.extend(element(0x0020, 0x000D, b"UI", UID.as_bytes()));
        assert_eq!(study_uid(&file("1.2.840.10008.1.2.1", &body)), Peek::Found(UID.into()));
    }

    #[test]
    fn implicit_little_endian() {
        let mut body = Vec::new();
        for (g, e, v) in [(0x0010u16, 0x0020u16, &b"P-001\0"[..]), (0x0020, 0x000D, UID.as_bytes())] {
            body.extend(g.to_le_bytes());
            body.extend(e.to_le_bytes());
            let mut v = v.to_vec();
            if v.len() % 2 == 1 {
                v.push(0);
            }
            body.extend((v.len() as u32).to_le_bytes());
            body.extend(v);
        }
        assert_eq!(study_uid(&file(IMPLICIT_LE, &body)), Peek::Found(UID.into()));
    }

    #[test]
    fn what_cannot_be_read_is_refused() {
        assert_eq!(study_uid(&[0u8; 300]), Peek::Unreadable, "no DICM");
        assert_eq!(study_uid(b"PK\x03\x04 a zip archive, not a DICOM file of course....................................................................................................................................."), Peek::Unreadable);
        let no_uid = file("1.2.840.10008.1.2.1", &element(0x0028, 0x0010, b"US", &[0, 2]));
        assert_eq!(study_uid(&no_uid), Peek::Unreadable, "past the UID without one");
        let bad_uid = file("1.2.840.10008.1.2.1", &element(0x0020, 0x000D, b"UI", b"1.2.*"));
        assert_eq!(study_uid(&bad_uid), Peek::Unreadable, "a wildcard is not a UID");
        let deflated = file(DEFLATED, &element(0x0020, 0x000D, b"UI", UID.as_bytes()));
        assert_eq!(study_uid(&deflated), Peek::Unreadable);
    }
}
