//! A schema-free protobuf reader for the tests: tags, wire types and
//! payloads, nothing of OTLP. Every field number a test looks up is written
//! in the test from the `.proto`.

/// A field's payload, by wire type.
#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    /// Wire type 0.
    Varint(u64),
    /// Wire type 1.
    Fixed64(u64),
    /// Wire type 2.
    Len(Vec<u8>),
}

/// One field of a message.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub number: u32,
    pub payload: Payload,
}

/// Every field of one message, in wire order; panics on a malformed buffer
/// and on wire types 3, 4 and 5 (groups and 32-bit fields: OTLP as dorfq
/// writes it has none).
pub fn fields(bytes: &[u8]) -> Vec<Field> {
    let mut at = 0;
    let mut fields = Vec::new();
    while at < bytes.len() {
        let key = read_varint(bytes, &mut at);
        let number = u32::try_from(key >> 3).expect("a field number fits 32 bits");
        let payload = match key & 7 {
            0 => Payload::Varint(read_varint(bytes, &mut at)),
            1 => {
                let word = take(bytes, &mut at, 8);
                Payload::Fixed64(u64::from_le_bytes(word.try_into().expect("eight bytes")))
            }
            2 => {
                let len = usize::try_from(read_varint(bytes, &mut at)).expect("a length fits");
                Payload::Len(take(bytes, &mut at, len).to_vec())
            }
            other => panic!("field {number}: unhandled wire type {other}"),
        };
        fields.push(Field { number, payload });
    }
    fields
}

fn read_varint(bytes: &[u8], at: &mut usize) -> u64 {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).expect("a varint ends before the buffer");
        *at += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return v;
        }
    }
    panic!("a varint longer than ten bytes");
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, len: usize) -> &'a [u8] {
    let end = at.checked_add(len).filter(|end| *end <= bytes.len());
    let end = end.expect("a payload ends before the buffer");
    let taken = &bytes[*at..end];
    *at = end;
    taken
}

/// Lookups over a message's fields.
pub trait Fields {
    /// The one field numbered `number`; panics on none or several.
    fn one(&self, number: u32) -> &Payload;
    /// Every field numbered `number`, in wire order.
    fn all(&self, number: u32) -> Vec<&Payload>;
    /// The field numbers present, in wire order.
    fn numbers(&self) -> Vec<u32>;
}

impl Fields for [Field] {
    fn one(&self, number: u32) -> &Payload {
        let all = self.all(number);
        assert_eq!(all.len(), 1, "exactly one field {number}");
        all[0]
    }

    fn all(&self, number: u32) -> Vec<&Payload> {
        self.iter()
            .filter(|field| field.number == number)
            .map(|field| &field.payload)
            .collect()
    }

    fn numbers(&self) -> Vec<u32> {
        self.iter().map(|field| field.number).collect()
    }
}

impl Payload {
    /// A length-delimited payload read as an embedded message.
    pub fn message(&self) -> Vec<Field> {
        fields(self.bytes())
    }

    /// A length-delimited payload read as UTF-8.
    pub fn string(&self) -> &str {
        std::str::from_utf8(self.bytes()).expect("a string is UTF-8")
    }

    /// A varint payload.
    pub fn varint(&self) -> u64 {
        match self {
            Payload::Varint(v) => *v,
            other => panic!("expected a varint, got {other:?}"),
        }
    }

    /// An eight-byte payload.
    pub fn fixed64(&self) -> u64 {
        match self {
            Payload::Fixed64(v) => *v,
            other => panic!("expected eight bytes, got {other:?}"),
        }
    }

    /// An eight-byte payload read as a `double`.
    pub fn double(&self) -> f64 {
        f64::from_bits(self.fixed64())
    }

    /// An eight-byte payload read as an `sfixed64`.
    pub fn sfixed64(&self) -> i64 {
        self.fixed64() as i64
    }

    /// A varint payload read as an `int64`.
    pub fn int64(&self) -> i64 {
        self.varint() as i64
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Payload::Len(bytes) => bytes,
            other => panic!("expected a length-delimited payload, got {other:?}"),
        }
    }
}
