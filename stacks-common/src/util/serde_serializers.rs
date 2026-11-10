/// Serde adapter for fixed-size byte arrays as lowercase hex without a prefix.
pub mod hex_array {
    /// Serialize an array as lowercase hex without a prefix.
    pub fn serialize<S: serde::Serializer, const N: usize>(
        val: &[u8; N],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.serialize_str(&crate::util::hash::to_hex(val))
    }

    /// Decode hex first, preserving hex errors before the custom size error.
    pub fn deserialize<'de, D: serde::Deserializer<'de>, const N: usize>(
        d: D,
    ) -> Result<[u8; N], D::Error> {
        let hex: String = serde::Deserialize::deserialize(d)?;
        let bytes = crate::util::hash::hex_bytes(&hex).map_err(serde::de::Error::custom)?;
        bytes
            .try_into()
            .map_err(|_| serde::de::Error::custom(format!("Invalid hex string -- not {N} bytes")))
    }
}

/// Skip any two-byte prefix, retaining legacy acceptance and UTF-8 boundary errors.
fn skip_two_char_prefix<E: serde::de::Error>(s: &str) -> Result<&str, E> {
    s.get(2..)
        .ok_or_else(|| E::invalid_length(s.len(), &"at least length 2 string"))
}

/// Check prefix length and UTF-8 boundary first, then require the literal `0x`.
fn strip_0x_prefix<E: serde::de::Error>(s: &str) -> Result<&str, E> {
    let hex = skip_two_char_prefix(s)?;
    if s.get(0..2) != Some("0x") {
        return Err(E::custom("must supply 0x-prefix"));
    }
    Ok(hex)
}

/// Serialize consensus bytes as lowercase hex with a `0x` prefix.
fn codec_to_prefixed_hex<T: crate::codec::StacksMessageCodec, E: serde::ser::Error>(
    val: &T,
) -> Result<String, E> {
    let mut bytes = vec![];
    val.consensus_serialize(&mut bytes).map_err(E::custom)?;
    Ok(crate::util::hash::to_hex_prefixed(&bytes, true))
}

/// Decode hex consensus bytes, preserving the codec's trailing-byte behavior.
fn codec_from_hex<T: crate::codec::StacksMessageCodec, E: serde::de::Error>(
    hex: &str,
) -> Result<T, E> {
    let bytes = crate::util::hash::hex_bytes(hex).map_err(E::custom)?;
    T::consensus_deserialize(&mut &bytes[..]).map_err(E::custom)
}

/// This module serde encodes and decodes optional byte fields in RPC
/// responses as Some(String) where the String is a `0x` prefixed
/// hex string.
/// Deserialization accepts any two-byte prefix for legacy compatibility.
pub mod prefix_opt_hex {
    pub fn serialize<S: serde::Serializer, T: std::fmt::LowerHex>(
        val: &Option<T>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match val {
            Some(ref some_val) => {
                let val_str = format!("0x{some_val:x}");
                s.serialize_some(&val_str)
            }
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>, T: crate::util::HexDeser>(
        d: D,
    ) -> Result<Option<T>, D::Error> {
        let opt_inst_str: Option<String> = serde::Deserialize::deserialize(d)?;
        let Some(inst_str) = opt_inst_str else {
            return Ok(None);
        };
        let hex_str = super::skip_two_char_prefix::<D::Error>(&inst_str)?;
        let val = T::try_from_hex(hex_str).map_err(serde::de::Error::custom)?;
        Ok(Some(val))
    }
}

/// This module serde encodes and decodes implementations of
/// `HexSer` and `HexDeser` in RPC responses as a String where the
/// String is a `0x` prefixed hex string.
/// Deserialization requires the literal `0x` prefix.
pub mod prefix_hex {
    pub fn serialize<S: serde::Serializer, T: crate::util::HexSer>(
        val: &T,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let val_fmter = std::fmt::from_fn(|f| T::fmt_hex(val, f));
        s.serialize_str(&format!("0x{val_fmter}"))
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>, T: crate::util::HexDeser>(
        d: D,
    ) -> Result<T, D::Error> {
        let inst_str: String = serde::Deserialize::deserialize(d)?;
        let hex_str = super::strip_0x_prefix::<D::Error>(&inst_str)?;
        T::try_from_hex(hex_str).map_err(serde::de::Error::custom)
    }
}

/// This module serde encode and decodes structs that
/// implement StacksMessageCodec as a 0x-prefixed hex string.
/// Deserialization accepts any two-byte prefix for legacy compatibility.
pub mod prefix_hex_codec {
    use crate::codec::StacksMessageCodec;

    pub fn serialize<S: serde::Serializer, T: StacksMessageCodec>(
        val: &T,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::codec_to_prefixed_hex::<_, S::Error>(val)?)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>, T: StacksMessageCodec>(
        d: D,
    ) -> Result<T, D::Error> {
        let inst_str: String = serde::Deserialize::deserialize(d)?;
        let hex_str = super::skip_two_char_prefix::<D::Error>(&inst_str)?;
        super::codec_from_hex(hex_str)
    }
}

/// This module serde encode and decodes structs that
/// implement StacksMessageCodec as a 0x-prefixed hex string.
/// This is the same as prefix_hex_codec, but for Option<T>.
/// Deserialization accepts any two-byte prefix for legacy compatibility.
pub mod prefix_opt_hex_codec {
    use crate::codec::StacksMessageCodec;

    pub fn serialize<S: serde::Serializer, T: StacksMessageCodec>(
        val: &Option<T>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match val {
            Some(ref some_val) => {
                let hex_string = super::codec_to_prefixed_hex::<_, S::Error>(some_val)?;
                s.serialize_some(&hex_string)
            }
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>, T: StacksMessageCodec>(
        d: D,
    ) -> Result<Option<T>, D::Error> {
        let opt_inst_str: Option<String> = serde::Deserialize::deserialize(d)?;
        let Some(inst_string) = opt_inst_str else {
            return Ok(None);
        };
        let hex_str = super::skip_two_char_prefix::<D::Error>(&inst_string)?;
        let val = super::codec_from_hex(hex_str)?;
        Ok(Some(val))
    }
}

/// Serialize strings as 0x-prefixed strings.
/// Deserialization accepts any two-byte prefix for legacy compatibility.
pub mod prefix_string_0x {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(val: &str, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("0x{val}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        let s: String = Deserialize::deserialize(d)?;
        let hex_str = super::skip_two_char_prefix::<D::Error>(&s)?;
        Ok(hex_str.to_string())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hex_characterization_json() {
        use crate::util::hash::{DoubleSha256, Hash160, Sha256Sum};
        fn check<T: serde::Serialize + serde::de::DeserializeOwned>(n: usize) {
            let json = format!("\"{}\"", "aB".repeat(n));
            let value: T = serde_json::from_str(&json).unwrap();
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                format!("\"{}\"", "ab".repeat(n))
            );
            for (input, expected) in [
                ("abcd", format!("Invalid hex string -- not {n} bytes")),
                ("zz", "bad character z for hex string".into()),
                ("a", "bad length 1 for hex string".into()),
                ("é0", "bad character é for hex string".into()),
            ] {
                let err = serde_json::from_str::<T>(&serde_json::to_string(input).unwrap())
                    .err()
                    .unwrap();
                assert_eq!(err.to_string(), expected);
            }
        }
        check::<Hash160>(20);
        check::<Sha256Sum>(32);
        check::<DoubleSha256>(32);
    }

    #[test]
    fn hex_characterization_prefixes() {
        // The legacy adapters skip two bytes without requiring an actual 0x prefix.
        for input in ["zzab", "0xab", "éab"] {
            let json = serde_json::to_string(input).unwrap();
            let mut d = serde_json::Deserializer::from_str(&json);
            let prefixed = format!("{}{}", &input[..2], "ab".repeat(20));
            let opt_json = serde_json::to_string(&prefixed).unwrap();
            let mut opt_d = serde_json::Deserializer::from_str(&opt_json);
            let value: Option<crate::util::hash::Hash160> =
                super::prefix_opt_hex::deserialize(&mut opt_d).unwrap();
            assert_eq!(value.unwrap().0, [0xab; 20]);
            assert_eq!(
                super::prefix_hex_codec::deserialize::<_, u8>(&mut d).unwrap(),
                0xab
            );
            let mut d = serde_json::Deserializer::from_str(&json);
            assert_eq!(
                super::prefix_opt_hex_codec::deserialize::<_, u8>(&mut d).unwrap(),
                Some(0xab)
            );
            let mut d = serde_json::Deserializer::from_str(&json);
            assert_eq!(super::prefix_string_0x::deserialize(&mut d).unwrap(), "ab");
        }
        for input in ["", "0", "aé"] {
            let json = serde_json::to_string(input).unwrap();
            macro_rules! short {
                ($module:ident, $ty:ty) => {
                    let mut d = serde_json::Deserializer::from_str(&json);
                    let result: Result<$ty, _> = super::$module::deserialize(&mut d);
                    assert!(result
                        .unwrap_err()
                        .to_string()
                        .contains("at least length 2 string"));
                };
            }
            short!(prefix_opt_hex, Option<crate::util::hash::Hash160>);
            short!(prefix_hex_codec, u8);
            short!(prefix_opt_hex_codec, Option<u8>);
            short!(prefix_string_0x, String);
            short!(prefix_hex, [u8; 1]);
        }
        assert!(deser_helper::<1>("zzab")
            .unwrap_err()
            .to_string()
            .contains("must supply 0x-prefix"));
    }

    fn ser_helper<const N: usize>(bytes: &[u8; N]) -> Result<String, serde_json::Error> {
        let mut ser = serde_json::Serializer::new(vec![]);
        super::prefix_hex::serialize(bytes, &mut ser)?;
        // read the json string (i.e., "\"0x00\"") into the hex string ("0x00")
        let out = serde_json::from_slice(&ser.into_inner()).unwrap();
        Ok(out)
    }

    fn deser_helper<const N: usize>(hex: &str) -> Result<[u8; N], serde_json::Error> {
        let json_str = format!("\"{hex}\"");
        let mut deser = serde_json::Deserializer::from_str(&json_str);
        super::prefix_hex::deserialize(&mut deser)
    }

    #[test]
    fn prefix_hex_byte_array_errors() {
        let err = deser_helper::<1>("0").unwrap_err();
        assert!(err.to_string().contains("at least length 2 string"));
        let err = deser_helper::<1>("00aa").unwrap_err();
        assert!(err.to_string().contains("must supply 0x-prefix"));
        let err = deser_helper::<1>("0x").unwrap_err();
        assert!(err.to_string().contains("bad length 0"));
        let err = deser_helper::<2>("0x00").unwrap_err();
        assert!(err.to_string().contains("bad length 2"));
        let err = deser_helper::<3>("0x000").unwrap_err();
        assert!(err.to_string().contains("bad length 3"));
        let err = deser_helper::<3>("0x00aaq1").unwrap_err();
        assert!(err.to_string().contains("bad character q for hex string"));
    }

    #[test]
    fn prefix_hex_byte_array_okay() {
        let n1_fixtures = [([0x00u8], "0x00"), ([0xff], "0xFf")];
        for (bytes, hex) in n1_fixtures.iter() {
            let out = deser_helper(hex).unwrap();
            assert_eq!(out, *bytes);
            let out = ser_helper(bytes).unwrap();
            assert_eq!(&out, &hex.to_ascii_lowercase());
        }
        let n5_fixtures = [
            ([0xff, 0xaa, 0xbc, 0x1f, 0x24], "0xffaabC1f24"),
            ([0xff, 0x00, 0xbc, 0x1f, 0x24], "0xff00Bc1f24"),
            ([0xff, 0xaa, 0xac, 0x0f, 0x24], "0xffaaac0F24"),
        ];
        for (bytes, hex) in n5_fixtures.iter() {
            let out = deser_helper(hex).unwrap();
            assert_eq!(out, *bytes);
            let out = ser_helper(bytes).unwrap();
            assert_eq!(&out, &hex.to_ascii_lowercase());
        }
        let out: [u8; 0] = deser_helper("0x").unwrap();
        let test: [u8; 0] = [];
        assert_eq!(out, test);
        let out = ser_helper(&test).unwrap();
        assert_eq!("0x", out);
    }
}
