//! D-Bus's type system, as much of it as this program sends and receives.
//!
//! **Four rules and six types.** Everything in a D-Bus body is aligned to its own
//! width from the start of the body; an array is a `u32` of *bytes* followed by
//! its elements, padded to the element's alignment before the first one; a struct
//! aligns to eight; and a variant is a signature followed by a value. That is the
//! whole of what is implemented here, because it is the whole of what
//! [ADR 0023](../../../../docs/adr/0023-d-bus-is-a-socket-we-speak-ourselves.md)
//! says we speak.
//!
//! **Little-endian only, on the wire we write.** The header carries the byte
//! order, so a peer may send either — and this reads both, because the bus daemon
//! is entitled to forward whatever a big-endian client sent.

/// One value, in the subset this program uses.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Value {
    /// `y`
    Byte(u8),
    /// `b`, four bytes on the wire and not one.
    Bool(bool),
    /// `u`
    U32(u32),
    /// `x`, which udisks2 uses for sizes.
    I64(i64),
    /// `s`
    Str(String),
    /// `o`, a string with a different signature and the same encoding.
    Path(String),
    /// `g`, a signature, whose length is one byte rather than four.
    Signature(String),
    /// `a` of one element type, which every array here has.
    Array(Vec<Value>),
    /// `a{sv}`, which is how every D-Bus interface takes options.
    Dict(Vec<(String, Value)>),
    /// `v`
    Variant(Box<Value>),
}

impl Value {
    /// The signature of this value, as the wire spells it.
    ///
    /// **An empty array's signature is a guess**, and the guess is `s`: every
    /// empty array this program sends is a list of strings or of paths, and the
    /// two marshal identically. A caller that needs another sends one element or
    /// says so in the signature it passes.
    #[must_use]
    pub fn signature(&self) -> String {
        match self {
            Self::Byte(_) => "y".to_owned(),
            Self::Bool(_) => "b".to_owned(),
            Self::U32(_) => "u".to_owned(),
            Self::I64(_) => "x".to_owned(),
            Self::Str(_) => "s".to_owned(),
            Self::Path(_) => "o".to_owned(),
            Self::Signature(_) => "g".to_owned(),
            Self::Array(items) => {
                format!("a{}", items.first().map_or_else(|| "s".to_owned(), Self::signature))
            }
            Self::Dict(_) => "a{sv}".to_owned(),
            Self::Variant(_) => "v".to_owned(),
        }
    }

    /// What this value aligns to.
    const fn alignment(&self) -> usize {
        match self {
            Self::Byte(_) | Self::Signature(_) | Self::Variant(_) => 1,
            Self::Bool(_) | Self::U32(_) | Self::Str(_) | Self::Path(_) => 4,
            Self::I64(_) | Self::Array(_) | Self::Dict(_) => 8,
        }
    }

    /// The string this holds, whatever kind of string it is.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Str(text) | Self::Path(text) | Self::Signature(text) => Some(text),
            Self::Variant(inner) => inner.text(),
            _ => None,
        }
    }

    /// The items this holds, if it is an array.
    #[must_use]
    pub fn items(&self) -> &[Self] {
        match self {
            Self::Array(items) => items,
            Self::Variant(inner) => inner.items(),
            _ => &[],
        }
    }

    /// The number this holds, however it is spelled.
    ///
    /// **Through a variant, the way [`Value::text`] is.** Every number that
    /// arrives from a bus arrives inside one — a header field, a property, a
    /// dictionary entry — and an accessor that stopped at the wrapper would
    /// answer `None` for the commonest shape there is. That is what it did, and
    /// the reply's serial went missing.
    #[must_use]
    pub fn number(&self) -> Option<i64> {
        match self {
            Self::Byte(byte) => Some(i64::from(*byte)),
            Self::U32(value) => Some(i64::from(*value)),
            Self::I64(value) => Some(*value),
            Self::Variant(inner) => inner.number(),
            _ => None,
        }
    }
}

/// How much padding takes `at` up to a multiple of `to`.
const fn padding(at: usize, to: usize) -> usize {
    (to - at % to) % to
}

/// The alignment of the type a signature starts with.
fn alignment_of(signature: &str) -> usize {
    match signature.as_bytes().first() {
        Some(b'y' | b'g' | b'v') => 1,
        Some(b'n' | b'q') => 2,
        Some(b'x' | b't' | b'd' | b'a' | b'(' | b'{') => 8,
        _ => 4,
    }
}

/// Writes values into a body.
#[derive(Default)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// A writer whose alignment is measured from `from` bytes in.
    ///
    /// **Alignment is from the start of the *message*, not of the buffer.** A
    /// body is marshalled after a header of some length, and a value that padded
    /// itself to its own offset in a fresh buffer would sit at the wrong place in
    /// the message — which is the bug this parameter exists to make impossible to
    /// write.
    #[must_use]
    pub fn at(from: usize) -> Self {
        Self { bytes: vec![0; from] }
    }

    /// Everything written since `at`.
    #[must_use]
    pub fn body(self, from: usize) -> Vec<u8> {
        self.bytes[from.min(self.bytes.len())..].to_vec()
    }

    /// The whole buffer, header included.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// How far in the writer is.
    ///
    /// Not `len`: what a caller wants from it is an offset to align against, and
    /// a writer with nothing in it is not a question anybody here asks.
    #[must_use]
    pub fn so_far(&self) -> usize {
        self.bytes.len()
    }

    fn pad(&mut self, to: usize) {
        self.bytes.resize(self.bytes.len() + padding(self.bytes.len(), to), 0);
    }

    /// Writes one value, padded to its own alignment.
    pub fn write(&mut self, value: &Value) {
        self.pad(value.alignment());
        match value {
            Value::Byte(byte) => self.bytes.push(*byte),
            Value::Bool(yes) => self.bytes.extend_from_slice(&u32::from(*yes).to_le_bytes()),
            Value::U32(number) => self.bytes.extend_from_slice(&number.to_le_bytes()),
            Value::I64(number) => self.bytes.extend_from_slice(&number.to_le_bytes()),
            Value::Str(text) | Value::Path(text) => self.write_string(text),
            Value::Signature(text) => {
                let length = u8::try_from(text.len()).unwrap_or(0);
                self.bytes.push(length);
                self.bytes.extend_from_slice(&text.as_bytes()[..length as usize]);
                self.bytes.push(0);
            }
            Value::Array(items) => {
                let element = items.first().map_or(1, Value::alignment);
                self.write_array(element, |writer| {
                    for item in items {
                        writer.write(item);
                    }
                });
            }
            Value::Dict(pairs) => {
                self.write_array(8, |writer| {
                    for (key, value) in pairs {
                        writer.pad(8);
                        writer.write(&Value::Str(key.clone()));
                        writer.write(&Value::Variant(Box::new(value.clone())));
                    }
                });
            }
            Value::Variant(inner) => {
                self.write(&Value::Signature(inner.signature()));
                self.write(inner);
            }
        }
    }

    /// A `u32` of bytes, then the elements, padded to the element's alignment
    /// **before** the length is known — which is why the length is written twice.
    fn write_array(&mut self, element: usize, fill: impl FnOnce(&mut Self)) {
        self.pad(4);
        let at = self.bytes.len();
        self.bytes.extend_from_slice(&0u32.to_le_bytes());
        self.pad(element);
        let from = self.bytes.len();
        fill(self);
        let length = u32::try_from(self.bytes.len() - from).unwrap_or(0);
        self.bytes[at..at + 4].copy_from_slice(&length.to_le_bytes());
    }

    fn write_string(&mut self, text: &str) {
        let length = u32::try_from(text.len()).unwrap_or(0);
        self.bytes.extend_from_slice(&length.to_le_bytes());
        self.bytes.extend_from_slice(text.as_bytes());
        self.bytes.push(0);
    }
}

/// Reads values out of a message.
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    little: bool,
}

impl<'a> Reader<'a> {
    /// A reader over a whole message, in the byte order the header declared.
    #[must_use]
    pub const fn new(bytes: &'a [u8], at: usize, little: bool) -> Self {
        Self { bytes, at, little }
    }

    /// How far in it has got.
    #[must_use]
    pub const fn at(&self) -> usize {
        self.at
    }

    /// Whether there is anything left.
    #[must_use]
    pub const fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn pad(&mut self, to: usize) {
        self.at += padding(self.at, to);
    }

    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let slice = self.bytes.get(self.at..self.at.checked_add(count)?)?;
        self.at += count;
        Some(slice)
    }

    fn u32(&mut self) -> Option<u32> {
        self.pad(4);
        let bytes: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(if self.little { u32::from_le_bytes(bytes) } else { u32::from_be_bytes(bytes) })
    }

    /// Reads one value of the type `signature` starts with.
    ///
    /// `None` for a type this build does not know, which is a message it did not
    /// understand — see the module header.
    pub fn read(&mut self, signature: &str) -> Option<Value> {
        let mut kinds = signature.chars();
        let kind = kinds.next()?;
        match kind {
            'y' => {
                let byte = *self.take(1)?.first()?;
                Some(Value::Byte(byte))
            }
            'b' => Some(Value::Bool(self.u32()? != 0)),
            'u' => Some(Value::U32(self.u32()?)),
            'x' | 't' => {
                self.pad(8);
                let bytes: [u8; 8] = self.take(8)?.try_into().ok()?;
                let number =
                    if self.little { i64::from_le_bytes(bytes) } else { i64::from_be_bytes(bytes) };
                Some(Value::I64(number))
            }
            's' | 'o' => {
                let length = self.u32()? as usize;
                let text = String::from_utf8_lossy(self.take(length)?).into_owned();
                self.take(1)?;
                Some(if kind == 's' { Value::Str(text) } else { Value::Path(text) })
            }
            'g' => {
                let length = *self.take(1)?.first()? as usize;
                let text = String::from_utf8_lossy(self.take(length)?).into_owned();
                self.take(1)?;
                Some(Value::Signature(text))
            }
            'v' => {
                let Value::Signature(inner) = self.read("g")? else { return None };
                Some(Value::Variant(Box::new(self.read(&inner)?)))
            }
            'a' => self.read_array(&signature[1..]),
            _ => None,
        }
    }

    /// An array, whose elements are all of `element`.
    fn read_array(&mut self, element: &str) -> Option<Value> {
        let length = self.u32()? as usize;
        let dict = element.starts_with('{');
        self.pad(if dict { 8 } else { alignment_of(element) });
        let end = self.at.checked_add(length)?;
        if end > self.bytes.len() {
            return None;
        }
        if dict {
            // `a{sv}` and nothing else: the key is a string in every dictionary
            // this program reads, and a message with another one is a message it
            // did not understand.
            let mut pairs = Vec::new();
            while self.at < end {
                self.pad(8);
                let key = self.read("s")?.text()?.to_owned();
                // **Unwrapped.** A dictionary's values are variants by
                // construction, so holding the wrapper as well would make the
                // same dictionary two different `Value`s depending on which end
                // of the wire it came from.
                let value = match self.read("v")? {
                    Value::Variant(inner) => *inner,
                    other => other,
                };
                pairs.push((key, value));
            }
            return Some(Value::Dict(pairs));
        }
        let mut items = Vec::new();
        while self.at < end {
            items.push(self.read(element)?);
        }
        Some(Value::Array(items))
    }
}

/// Every type in one signature, split into the values it names.
///
/// `"sas"` is a string and an array of strings; `"a{sv}s"` is a dictionary and a
/// string. Splitting is what the body reader needs and what a nested signature
/// makes non-trivial.
#[must_use]
pub fn split(signature: &str) -> Vec<String> {
    let bytes = signature.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let from = at;
        // An `a` binds to whatever follows it, including another `a`.
        while bytes.get(at) == Some(&b'a') {
            at += 1;
        }
        match bytes.get(at) {
            Some(b'(' | b'{') => {
                let (open, close) = if bytes[at] == b'(' { (b'(', b')') } else { (b'{', b'}') };
                let mut depth = 0;
                while at < bytes.len() {
                    if bytes[at] == open {
                        depth += 1;
                    } else if bytes[at] == close {
                        depth -= 1;
                        if depth == 0 {
                            at += 1;
                            break;
                        }
                    }
                    at += 1;
                }
            }
            Some(_) => at += 1,
            None => break,
        }
        out.push(signature[from..at].to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Reader, Value, Writer, split};

    /// A string, byte for byte, against the specification.
    ///
    /// `s` is a `u32` length, the bytes, and a NUL — and the length does not count
    /// the NUL. Written out by hand because a marshaller tested only against its
    /// own reader agrees with itself and with nobody else.
    #[test]
    fn a_string_is_a_length_the_bytes_and_a_nul() {
        let mut writer = Writer::at(0);
        writer.write(&Value::Str("foo".to_owned()));
        assert_eq!(writer.into_bytes(), [3, 0, 0, 0, b'f', b'o', b'o', 0]);

        let mut writer = Writer::at(0);
        writer.write(&Value::Signature("as".to_owned()));
        assert_eq!(writer.into_bytes(), [2, b'a', b's', 0], "a signature counts in one byte");
    }

    /// Alignment is from the start of the message, and a `u32` after one byte
    /// sits three bytes further on.
    #[test]
    fn a_value_is_padded_to_its_own_width_from_the_message_start() {
        let mut writer = Writer::at(0);
        writer.write(&Value::Byte(7));
        writer.write(&Value::U32(1));
        assert_eq!(writer.into_bytes(), [7, 0, 0, 0, 1, 0, 0, 0]);

        // The same value, in a body that starts one byte into the message: the
        // padding is different, which is the whole reason `at` exists.
        let mut writer = Writer::at(1);
        writer.write(&Value::U32(1));
        assert_eq!(writer.body(1), [0, 0, 0, 1, 0, 0, 0]);
    }

    /// An array is a length in *bytes*, then padding to the element, then the
    /// elements — and an empty one still pads.
    #[test]
    fn an_array_counts_bytes_and_pads_before_its_first_element() {
        let mut writer = Writer::at(0);
        writer.write(&Value::Array(vec![Value::U32(1), Value::U32(2)]));
        assert_eq!(writer.into_bytes(), [8, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0]);

        let mut writer = Writer::at(0);
        writer.write(&Value::Array(vec![Value::I64(1)]));
        // Four bytes of length, four of padding to the element's eight, then it.
        assert_eq!(writer.into_bytes(), [8, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
    }

    /// Everything this program sends survives its own reader, including the
    /// shapes with a nested signature.
    #[test]
    fn every_value_this_program_writes_reads_back_as_itself() {
        let values = [
            Value::Byte(255),
            Value::Bool(true),
            Value::U32(4_000_000_000),
            Value::I64(-9),
            Value::Str("a string with a space".to_owned()),
            Value::Path("/org/freedesktop/UDisks2".to_owned()),
            Value::Array(vec![Value::Str("one".to_owned()), Value::Str("two".to_owned())]),
            Value::Variant(Box::new(Value::Str("inside".to_owned()))),
            Value::Dict(vec![
                ("auth.no-user-interaction".to_owned(), Value::Bool(true)),
                ("size".to_owned(), Value::I64(4096)),
            ]),
        ];
        for value in values {
            let mut writer = Writer::at(0);
            writer.write(&value);
            let bytes = writer.into_bytes();
            let mut reader = Reader::new(&bytes, 0, true);
            let read = reader.read(&value.signature()).expect("it would not read back");
            assert_eq!(read, value);
            assert!(reader.done(), "{value:?} left {} bytes", bytes.len() - reader.at());
        }
    }

    /// An empty array is a length of zero and no elements, and reads back as one.
    #[test]
    fn an_empty_array_round_trips() {
        let mut writer = Writer::at(0);
        writer.write(&Value::Array(Vec::new()));
        let bytes = writer.into_bytes();
        assert_eq!(bytes, [0, 0, 0, 0]);
        let mut reader = Reader::new(&bytes, 0, true);
        assert_eq!(reader.read("as"), Some(Value::Array(Vec::new())));
    }

    /// A truncated message is a message we did not understand, not a panic.
    #[test]
    fn a_short_message_reads_as_nothing() {
        for bytes in [vec![], vec![3, 0, 0], vec![9, 0, 0, 0, b'x'], vec![255, 255, 255, 255]] {
            let mut reader = Reader::new(&bytes, 0, true);
            assert_eq!(reader.read("s"), None, "{bytes:?}");
        }
        // An array claiming more bytes than the message holds.
        let bytes = [200, 0, 0, 0, 1, 0, 0, 0];
        let mut reader = Reader::new(&bytes, 0, true);
        assert_eq!(reader.read("au"), None);
    }

    /// Signatures split into the values they name, nesting included.
    #[test]
    fn a_signature_splits_into_one_entry_per_value() {
        assert_eq!(split("sas"), ["s", "as"]);
        assert_eq!(split("a{sv}s"), ["a{sv}", "s"]);
        assert_eq!(split("aay"), ["aay"]);
        assert_eq!(split("(su)o"), ["(su)", "o"]);
        assert_eq!(split(""), Vec::<String>::new());
    }

    /// A big-endian peer is legal, and the bus is entitled to forward one.
    #[test]
    fn a_message_from_a_big_endian_peer_reads() {
        let bytes = [0, 0, 0, 3, b'f', b'o', b'o', 0];
        let mut reader = Reader::new(&bytes, 0, false);
        assert_eq!(reader.read("s"), Some(Value::Str("foo".to_owned())));
    }
}
