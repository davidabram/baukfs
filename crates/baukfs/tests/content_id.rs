use baukfs::{ContentId, ContentIdError};

/// BLAKE3 of the empty input, from the BLAKE3 reference test vectors.
const EMPTY: &str = "b3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";

#[test]
fn content_id_is_the_blake3_digest_of_the_exact_bytes() {
    let empty = ContentId::of(b"");
    assert_eq!(empty.to_string(), EMPTY);
    assert_eq!(empty.digest(), blake3::hash(b"").as_bytes());

    let inputs: [&[u8]; 5] = [b"a", b"a\n", b"a\r\n", b"A", b"a\0"];
    let ids: Vec<ContentId> = inputs.iter().map(|bytes| ContentId::of(bytes)).collect();
    for (index, id) in ids.iter().enumerate() {
        assert!(!ids[index + 1..].contains(id), "{id} repeats");
    }
}

#[test]
fn canonical_text_round_trips() {
    let id = ContentId::of(b"hello");
    let text = id.to_string();
    assert_eq!(text.len(), 3 + 64);
    assert!(text.starts_with("b3:"));
    assert!(
        text[3..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    assert_eq!(ContentId::parse(&text), Ok(id));
    assert_eq!(text.parse::<ContentId>(), Ok(id));
    assert_eq!(ContentId::parse(EMPTY), Ok(ContentId::of(b"")));
    assert_eq!(format!("{id:?}"), format!("ContentId({text})"));
}

#[test]
fn malformed_text_is_rejected_with_the_first_defect() {
    let hex = &EMPTY[3..];
    let upper = format!("b3:{}", hex.to_ascii_uppercase());
    let cases: [(String, ContentIdError); 14] = [
        (String::new(), ContentIdError::MissingPrefix),
        (hex.to_owned(), ContentIdError::MissingPrefix),
        (format!("B3:{hex}"), ContentIdError::MissingPrefix),
        (format!("blake3:{hex}"), ContentIdError::MissingPrefix),
        (format!(" b3:{hex}"), ContentIdError::MissingPrefix),
        (
            "b3:".to_owned(),
            ContentIdError::InvalidLength { digits: 0 },
        ),
        (
            format!("b3:{}", &hex[..63]),
            ContentIdError::InvalidLength { digits: 63 },
        ),
        (
            format!("b3:{hex}0"),
            ContentIdError::InvalidLength { digits: 65 },
        ),
        (
            format!("b3:{hex}{hex}"),
            ContentIdError::InvalidLength { digits: 128 },
        ),
        (
            upper,
            ContentIdError::InvalidCharacter {
                offset: 3,
                character: 'A',
            },
        ),
        (
            format!("b3:{}g", &hex[..63]),
            ContentIdError::InvalidCharacter {
                offset: 66,
                character: 'g',
            },
        ),
        (
            format!("b3:{hex} "),
            ContentIdError::InvalidCharacter {
                offset: 67,
                character: ' ',
            },
        ),
        (
            format!("b3:0x{}", &hex[2..]),
            ContentIdError::InvalidCharacter {
                offset: 4,
                character: 'x',
            },
        ),
        (
            format!("b3:{}é", &hex[..62]),
            ContentIdError::InvalidCharacter {
                offset: 65,
                character: 'é',
            },
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(ContentId::parse(&text), Err(expected), "{text:?}");
        assert_eq!(text.parse::<ContentId>(), Err(expected), "{text:?}");
    }
}
